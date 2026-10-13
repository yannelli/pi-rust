//! Token estimates for transcripts, ported from upstream `utils/estimate.ts`. Text counts in
//! UTF-16 units at 3.5 characters per token, so estimates match the TypeScript pi.

use serde_json::Value;

use crate::transcript::system_message_text;
use crate::types::{ContentBlock, Message, StopReason, Usage, UserContent};

const CHARS_PER_TOKEN: f64 = 3.5;
const ESTIMATED_IMAGE_CHARS: usize = 4800;

/// Estimated context size, anchored on the most recent applicable assistant usage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextUsageEstimate {
    pub tokens: u64,
    /// Tokens reported by the assistant usage the estimate starts from.
    pub usage_tokens: u64,
    /// Estimated tokens after that assistant message.
    pub trailing_tokens: u64,
    /// Index of the message whose usage was used.
    pub last_usage_index: Option<usize>,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn tokens_for_chars(chars: usize) -> u64 {
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64
}

/// Total tokens of a usage report, from its components when `total_tokens` is zero.
pub fn calculate_context_tokens(usage: &Usage) -> u64 {
    match usage.total_tokens {
        0 => usage.input + usage.output + usage.cache_read + usage.cache_write,
        total => total,
    }
}

pub fn estimate_text_tokens(text: &str) -> u64 {
    tokens_for_chars(utf16_len(text))
}

fn content_chars(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => utf16_len(text),
            ContentBlock::Image { .. } => ESTIMATED_IMAGE_CHARS,
            ContentBlock::Thinking { thinking, .. } => utf16_len(thinking),
            ContentBlock::ToolCall(call) => {
                utf16_len(&call.name) + utf16_len(&Value::Object(call.arguments.clone()).to_string())
            }
        })
        .sum()
}

/// Estimated tokens of user or tool result content; images count as 4800 characters.
pub fn estimate_content_tokens(content: &UserContent) -> u64 {
    match content {
        UserContent::Text(text) => estimate_text_tokens(text),
        UserContent::Blocks(blocks) => tokens_for_chars(content_chars(blocks)),
    }
}

fn estimate_json_tokens<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_string(value).map(|json| estimate_text_tokens(&json)).unwrap_or_default()
}

pub fn estimate_message_tokens(message: &Message) -> u64 {
    match message {
        Message::System(system) => {
            let tools_added = system.tools_added.as_ref().filter(|tools| !tools.is_empty());
            let tools_removed = system.tools_removed.as_ref().filter(|tools| !tools.is_empty());
            estimate_text_tokens(&system_message_text(system))
                + tools_added.map_or(0, estimate_json_tokens)
                + tools_removed.map_or(0, estimate_json_tokens)
        }
        Message::User(user) => estimate_content_tokens(&user.content),
        Message::ToolResult(result) => tokens_for_chars(content_chars(&result.content)),
        Message::Assistant(assistant) => tokens_for_chars(content_chars(&assistant.content)),
        Message::Custom(custom) => match custom.fields.get("content") {
            Some(content) => serde_json::from_value::<UserContent>(content.clone())
                .map(|content| estimate_content_tokens(&content))
                .unwrap_or_default(),
            None => 0,
        },
    }
}

fn message_timestamp(message: &Message) -> i64 {
    match message {
        Message::System(message) => message.timestamp,
        Message::User(message) => message.timestamp,
        Message::Assistant(message) => message.timestamp,
        Message::ToolResult(message) => message.timestamp,
        Message::Custom(message) => message.timestamp(),
    }
}

/// The last assistant usage that still describes the transcript prefix: a message inserted
/// later with a newer timestamp (such as a compaction summary) invalidates older usage.
fn last_usage(messages: &[Message]) -> Option<(usize, u64)> {
    let mut latest_prefix = i64::MIN;
    let mut found = None;
    for (index, message) in messages.iter().enumerate() {
        if let Message::Assistant(assistant) = message {
            let tokens = calculate_context_tokens(&assistant.usage);
            let applies = assistant.timestamp >= latest_prefix
                && !matches!(assistant.stop_reason, StopReason::Aborted | StopReason::Error)
                && tokens > 0;
            if applies {
                found = Some((index, tokens));
            }
        }
        latest_prefix = latest_prefix.max(message_timestamp(message));
    }
    found
}

pub fn estimate_context_tokens(messages: &[Message]) -> ContextUsageEstimate {
    match last_usage(messages) {
        Some((index, usage_tokens)) => {
            let trailing_tokens: u64 = messages[index + 1..].iter().map(estimate_message_tokens).sum();
            ContextUsageEstimate {
                tokens: usage_tokens + trailing_tokens,
                usage_tokens,
                trailing_tokens,
                last_usage_index: Some(index),
            }
        }
        None => {
            let tokens = messages.iter().map(estimate_message_tokens).sum();
            ContextUsageEstimate { tokens, usage_tokens: 0, trailing_tokens: tokens, last_usage_index: None }
        }
    }
}
