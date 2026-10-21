//! Context overflow detection from provider errors, ported from the TypeScript pi patterns.

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{AssistantMessage, StopReason};

/// Lowercase fragments that identify an overflow error across providers.
const OVERFLOW_FRAGMENTS: &[&str] = &[
    "prompt is too long",
    "prompt too long",
    "prompt exceeds max length",
    "request_too_large",
    "input is too long for requested model",
    "exceeds the context window",
    "maximum context length",
    "input token count",
    "maximum prompt length is",
    "reduce the length of the messages",
    "maximum allowed input length",
    "longer than the model's context length",
    "longer than the models context length",
    "exceeds the available context size",
    "greater than the context length",
    "context window exceeds limit",
    "exceeded model token limit",
    "configured context size is",
    "model_context_window_exceeded",
    "exceeded max context length",
    "exceeded context length",
    "range of input length should be",
    "context_length_exceeded",
    "context length exceeded",
    "too many tokens",
    "token limit exceeded",
];

const NON_OVERFLOW_FRAGMENTS: &[&str] =
    &["rate limit", "too many requests", "throttling error:", "service unavailable:"];

/// HTTP 413 is a request-size overflow for every provider, whatever the body says. This matches
/// the `<provider> API error (413):`, `Provider HTTP 413`, and bare `413` prefixes, not a status
/// mentioned later in the body.
static HTTP_413: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(?:[A-Za-z0-9_ -]+ API error \(413\):|Provider HTTP 413(?:\s|:|$)|413(?:\s|:|$))")
        .expect("valid 413 pattern")
});

/// Cerebras answers an oversized request with a bare `400 (no body)`; the bare `413` form is
/// covered by [`HTTP_413`].
fn is_cerebras_bodyless_overflow(error: &str) -> bool {
    let Some(rest) = error.strip_prefix("400") else { return false };
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("status code").map_or(rest, str::trim_start);
    rest.starts_with("(no body)")
}

/// Whether `message` failed, or silently overflowed, because the input exceeded the context
/// window: a provider overflow error, a successful response whose input exceeds the window,
/// or a `length` stop with no output and input filling the window.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = &message.error_message
    {
        let lower = error.to_lowercase();
        if !NON_OVERFLOW_FRAGMENTS.iter().any(|fragment| lower.contains(fragment))
            && (OVERFLOW_FRAGMENTS.iter().any(|fragment| lower.contains(fragment))
                || HTTP_413.is_match(error)
                || (message.provider == "cerebras" && is_cerebras_bodyless_overflow(&lower)))
        {
            return true;
        }
    }
    let Some(window) = context_window.filter(|window| *window > 0) else { return false };
    let input = message.usage.input + message.usage.cache_read;
    match message.stop_reason {
        StopReason::Stop => input > window,
        StopReason::Length => message.usage.output == 0 && input as f64 >= window as f64 * 0.99,
        _ => false,
    }
}

/// A `length` stop below the caller's or model's intended output limit (before any
/// context-based clamping), which one compact-and-retry may fix.
pub fn is_recoverable_length(message: &AssistantMessage, desired_max_output: u64) -> bool {
    message.stop_reason == StopReason::Length && desired_max_output > 0 && message.usage.output < desired_max_output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::faux::FauxProvider;

    fn failed(error: &str) -> AssistantMessage {
        let mut message = AssistantMessage::empty(&FauxProvider::model());
        message.stop_reason = StopReason::Error;
        message.error_message = Some(error.into());
        message
    }

    #[test]
    fn detects_provider_overflow_errors() {
        assert!(is_context_overflow(
            &failed("Provider HTTP 400: prompt is too long: 210000 tokens > 200000 maximum"),
            None
        ));
        assert!(is_context_overflow(&failed("Your input exceeds the context window of this model."), None));
        assert!(!is_context_overflow(&failed("Rate limit reached: too many tokens per minute"), None));
        assert!(!is_context_overflow(&failed("Provider HTTP 500: internal error"), None));
        let mut silent = AssistantMessage::empty(&FauxProvider::model());
        silent.stop_reason = StopReason::Stop;
        silent.usage.input = 200;
        assert!(is_context_overflow(&silent, Some(100)));
        assert!(!is_context_overflow(&silent, Some(0)));
    }

    #[test]
    fn detects_length_stop_overflow_and_recoverable_length() {
        let mut full = AssistantMessage::empty(&FauxProvider::model());
        full.stop_reason = StopReason::Length;
        full.usage.input = 99;
        assert!(is_context_overflow(&full, Some(100)) && !is_context_overflow(&full, Some(200)));
        full.usage.output = 1;
        assert!(!is_context_overflow(&full, Some(100)));
        assert!(is_recoverable_length(&full, 100) && !is_recoverable_length(&full, 1));
        assert!(!is_recoverable_length(&full, 0));
        let mut cerebras = failed("400 status code (no body)");
        assert!(!is_context_overflow(&cerebras, None));
        cerebras.provider = "cerebras".into();
        assert!(is_context_overflow(&cerebras, None));
        cerebras.error_message = Some("413 (no body)".into());
        assert!(is_context_overflow(&cerebras, None));
    }

    #[test]
    fn treats_http_413_as_request_size_overflow_for_any_provider() {
        for error in [
            r#"github-copilot API error (413): {"message":"failed to parse request","code":""}"#,
            "custom-provider API error (413): Payload Too Large",
            "Provider HTTP 413: failed to parse request",
            "Provider HTTP 413",
            r#"413 {"error":{"message":"failed to parse request","code":""}}"#,
            "413",
            "413 status code (no body)",
        ] {
            assert!(is_context_overflow(&failed(error), Some(1_000_000)), "{error}");
            assert!(is_context_overflow(&failed(error), None), "{error}");
        }
    }

    #[test]
    fn ignores_other_statuses_and_413_outside_the_prefix() {
        for error in [
            r#"github-copilot API error (400): {"message":"failed to parse request","code":""}"#,
            "custom-provider API error (429): Too many requests",
            "Provider HTTP 400: failed to parse request",
            r#"github-copilot API error (400): {"message":"failed to parse request: invalid value 413"}"#,
            "custom-provider API error (400): upstream mentioned OpenAI API error (413): invalid input",
            "Provider HTTP 400: upstream said Provider HTTP 413",
            "custom-provider API error (4130): invalid input",
            "Provider HTTP 4130: invalid input",
            "4130 failed to parse request",
            "400 status code (no body)",
        ] {
            assert!(!is_context_overflow(&failed(error), None), "{error}");
        }
    }

    #[test]
    fn limits_http_413_matching_to_failed_responses() {
        let mut message = failed("OpenAI API error (413): failed to parse request");
        for stop_reason in [StopReason::Aborted, StopReason::Stop] {
            message.stop_reason = stop_reason;
            assert!(!is_context_overflow(&message, None));
        }
    }
}
