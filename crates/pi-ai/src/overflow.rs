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

/// Whether `message` failed, or silently overflowed, because the input exceeded the context window.
pub fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool {
    if message.stop_reason == StopReason::Error
        && let Some(error) = &message.error_message
    {
        let error = error.to_lowercase();
        if !NON_OVERFLOW_FRAGMENTS.iter().any(|fragment| error.contains(fragment))
            && OVERFLOW_FRAGMENTS.iter().any(|fragment| error.contains(fragment))
        {
            return true;
        }
    }
    matches!(context_window, Some(window) if message.stop_reason == StopReason::Stop && message.usage.input + message.usage.cache_read > window)
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
    }
}
