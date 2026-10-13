//! Usage estimates of upstream `faux.ts`: prompt and output tokens from text length (4 characters per token, in
//! UTF-16 code units like JavaScript), and simulated prompt caching per session id.

use std::collections::HashMap;

use pi_ai::{AssistantMessage, CacheRetention, ContentBlock, Cost, Message, ToolResultMessage, Usage, UserContent};
use serde::Serialize;

/// UTF-16 length of `text`, as JavaScript's `String.length`.
pub(super) fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

pub(super) fn estimate_tokens(text: &str) -> u64 {
    js_length(text).div_ceil(4) as u64
}

/// `JSON.stringify(value)`.
pub(super) fn json_stringify(value: &impl Serialize) -> String {
    let options = pi_chord::CopyJsonOptions { omit_undefined_properties: true };
    pi_chord::copy_json(value, options).map(|json| json.to_string()).unwrap_or_default()
}

fn block_to_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text { text, .. } => text.clone(),
        ContentBlock::Thinking { thinking, .. } => thinking.clone(),
        ContentBlock::Image { data, mime_type } => format!("[image:{mime_type}:{}]", js_length(data)),
        ContentBlock::ToolCall(call) => format!("{}:{}", call.name, json_stringify(&call.arguments)),
    }
}

fn blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks.iter().map(block_to_text).collect::<Vec<_>>().join("\n")
}

fn tool_result_to_text(message: &ToolResultMessage) -> String {
    std::iter::once(message.tool_name.clone())
        .chain(message.content.iter().map(block_to_text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text of one message. App-defined roles never reach providers upstream; their fields stand in here.
fn message_to_text(message: &Message) -> String {
    match message {
        Message::System(system) => {
            let mut parts = vec![pi_ai::transcript::system_message_text(system)];
            let removed = system.tools_removed.iter().flatten().map(|tool| format!("tool-:{}", json_stringify(tool)));
            let added = system.tools_added.iter().flatten().map(|tool| format!("tool+:{}", json_stringify(tool)));
            parts.extend(removed.chain(added));
            parts.retain(|part| !part.is_empty());
            parts.join("\n")
        }
        Message::User(user) => match &user.content {
            UserContent::Text(text) => text.clone(),
            UserContent::Blocks(blocks) => blocks_to_text(blocks),
        },
        Message::Assistant(assistant) => blocks_to_text(&assistant.content),
        Message::ToolResult(result) => tool_result_to_text(result),
        Message::Custom(custom) => json_stringify(&custom.fields),
    }
}

pub(super) fn assistant_content_to_text(message: &AssistantMessage) -> String {
    blocks_to_text(&message.content)
}

/// Length of the prompt text that joins the first `count` of `messages` with blank lines.
fn joined_length(messages: &[Vec<u16>], count: usize) -> usize {
    let separators = if count > 0 { (count - 1) * 2 } else { 0 };
    separators + messages[..count].iter().map(Vec::len).sum::<usize>()
}

/// Length of the common prefix of two joined prompts: whole equal messages, then characters from the first
/// message that differs.
fn common_prompt_prefix_length(previous: &[Vec<u16>], current: &[Vec<u16>]) -> usize {
    let mut index = 0;
    while index < previous.len() && index < current.len() && previous[index] == current[index] {
        index += 1;
    }
    let rest = |messages: &[Vec<u16>]| -> Vec<u16> {
        if index == messages.len() {
            return Vec::new();
        }
        let mut joined: Vec<u16> = if index > 0 { vec![b'\n'.into(), b'\n'.into()] } else { Vec::new() };
        for (offset, message) in messages[index..].iter().enumerate() {
            if offset > 0 {
                joined.extend([u16::from(b'\n'), u16::from(b'\n')]);
            }
            joined.extend(message);
        }
        joined
    };
    let (left, right) = (rest(previous), rest(current));
    let common = left.iter().zip(&right).take_while(|(a, b)| a == b).count();
    joined_length(previous, index) + common
}

/// Prompt texts by session id, for simulated caching.
pub(super) type PromptCache = HashMap<String, Vec<Vec<u16>>>;

/// Replace `message.usage` with an estimate for `messages` and the response content.
pub(super) fn with_usage_estimate(
    mut message: AssistantMessage,
    messages: &[Message],
    session_id: Option<&str>,
    cache_retention: Option<CacheRetention>,
    cache: &mut PromptCache,
) -> AssistantMessage {
    let prompt: Vec<Vec<u16>> = messages
        .iter()
        .map(|message| format!("{}:{}", message.role(), message_to_text(message)).encode_utf16().collect())
        .collect();
    let prompt_length = joined_length(&prompt, prompt.len());
    let prompt_tokens = prompt_length.div_ceil(4) as u64;
    let output = estimate_tokens(&assistant_content_to_text(&message));
    let (mut input, mut cache_read, mut cache_write) = (prompt_tokens, 0, 0);
    if let Some(session) = session_id.filter(|session| !session.is_empty())
        && cache_retention != Some(CacheRetention::None)
    {
        match cache.get(session) {
            Some(previous) => {
                let cached = common_prompt_prefix_length(previous, &prompt);
                cache_read = cached.div_ceil(4) as u64;
                cache_write = (prompt_length - cached).div_ceil(4) as u64;
                input = prompt_tokens.saturating_sub(cache_read);
            }
            None => cache_write = prompt_tokens,
        }
        cache.insert(session.to_string(), prompt);
    }
    message.usage = Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: Cost::default(),
        ..Usage::default()
    };
    message
}
