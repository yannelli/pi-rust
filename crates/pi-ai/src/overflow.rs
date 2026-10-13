//! Context overflow detection from provider errors, ported from the TypeScript pi patterns.

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

/// Cerebras answers an oversized request with a bare `400 (no body)` or `413 (no body)`.
fn is_cerebras_bodyless_overflow(error: &str) -> bool {
    let rest = match error.strip_prefix("400").or_else(|| error.strip_prefix("413")) {
        Some(rest) => rest.trim_start(),
        None => return false,
    };
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
}
