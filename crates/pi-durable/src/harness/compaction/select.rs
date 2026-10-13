//! Range selection, context size estimates, and the summarizer's transcript (spec section 8.7).

use pi_ai::estimate::{calculate_context_tokens, estimate_message_tokens};
use pi_ai::{ContentBlock, Message, UserContent};

use crate::harness::context::order_tool_results;
use crate::harness::types::ContextView;

/// Longest tool result text a serialized summary source keeps.
const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Index in `view.entries` of the first entry a summary keeps, or `None` when there is nothing to compact.
/// Walks back from the tail until `keep_recent_tokens` are kept, then cuts at the first candidate at or after
/// that entry.
pub fn select_cut(view: &ContextView, keep_recent_tokens: u64) -> Option<usize> {
    let contributions = &view.contributions;
    let start = usize::from(view.head.is_some());
    let candidates: Vec<usize> =
        (start..contributions.len()).filter(|&index| is_candidate(contributions, index)).collect();
    let mut kept = 0;
    let mut cut = None;
    for index in (start..contributions.len()).rev() {
        kept += contributions[index].iter().map(estimate_message_tokens).sum::<u64>();
        if kept < keep_recent_tokens {
            continue;
        }
        cut = candidates.iter().copied().find(|&candidate| candidate >= index).or(candidates.last().copied());
        break;
    }
    let cut = cut?;
    (start..cut).any(|index| !contributions[index].is_empty()).then_some(cut)
}

fn is_candidate(contributions: &[Vec<Message>], index: usize) -> bool {
    match contributions[index].first() {
        Some(Message::Assistant(_)) => return true,
        Some(Message::User(_)) => {}
        _ => return false,
    }
    // A result of the preceding assistant's calls that follows this entry, before the next assistant, belongs before it.
    let mut calls: Vec<String> = Vec::new();
    for before in (0..index).rev() {
        let assistant = contributions[before].iter().rev().find_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant),
            _ => None,
        });
        if let Some(assistant) = assistant {
            calls = assistant.tool_calls().map(|call| call.id.clone()).collect();
            break;
        }
    }
    if calls.is_empty() {
        return true;
    }
    for (after, contribution) in contributions.iter().enumerate().skip(index) {
        for (position, message) in contribution.iter().enumerate() {
            match message {
                Message::Assistant(_) if after > index || position > 0 => return true,
                Message::ToolResult(result) if calls.contains(&result.tool_call_id) => return false,
                _ => {}
            }
        }
    }
    true
}

/// Model messages of the entries before `cut`: the head marker first, ordered like model context.
pub fn summarized_messages(view: &ContextView, cut: usize) -> Vec<Message> {
    let flat: Vec<Message> = view.contributions[..cut].iter().flatten().cloned().collect();
    order_tool_results(&flat)
}

/// Size of a request over `view` followed by `extra` (spec section 8.3): the usage of the newest assistant after
/// the head marker plus estimates of the messages after it; without one, estimates of every message.
pub fn estimate_context(view: &ContextView, extra: &[Message]) -> u64 {
    let after = view.head.as_ref().map(|head| head.id);
    let mut measured = None;
    for index in (0..view.entries.len()).rev() {
        if after.is_some_and(|after| view.entries[index].id <= after) {
            continue;
        }
        measured = view.contributions[index].iter().rev().find(|message| {
            matches!(message, Message::Assistant(assistant) if calculate_context_tokens(&assistant.usage) > 0)
        });
        if measured.is_some() {
            break;
        }
    }
    let (from, mut tokens) = match measured {
        Some(message @ Message::Assistant(assistant)) => {
            let position = view.messages.iter().rposition(|m| m == message).map_or(0, |at| at + 1);
            (position, calculate_context_tokens(&assistant.usage))
        }
        _ => (0, 0),
    };
    tokens += view.messages[from.min(view.messages.len())..].iter().map(estimate_message_tokens).sum::<u64>();
    tokens + extra.iter().map(estimate_message_tokens).sum::<u64>()
}

fn content_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks_text(blocks),
    }
}

fn blocks_text(blocks: &[ContentBlock]) -> String {
    blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

fn truncate(text: &str, max_chars: usize) -> String {
    let count = text.encode_utf16().count();
    if count <= max_chars {
        return text.to_owned();
    }
    let kept: Vec<u16> = text.encode_utf16().take(max_chars).collect();
    format!("{}\n\n[... {} more characters truncated]", String::from_utf16_lossy(&kept), count - max_chars)
}

/// Messages as plain text, so the summarizer reads a transcript instead of continuing it.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = content_text(&user.content);
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking = Vec::new();
                let mut text = Vec::new();
                let mut calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking { thinking: value, .. } => thinking.push(value.clone()),
                        ContentBlock::Text { text: value, .. } => text.push(value.clone()),
                        ContentBlock::ToolCall(call) => {
                            let args: Vec<String> = call
                                .arguments
                                .iter()
                                .map(|(key, value)| format!("{key}={}", pi_chord::Json::from(value)))
                                .collect();
                            calls.push(format!("{}({})", call.name, args.join(", ")));
                        }
                        ContentBlock::Image { .. } => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if !text.is_empty() {
                    parts.push(format!("[Assistant]: {}", text.join("\n")));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = blocks_text(&result.content);
                if !text.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate(&text, TOOL_RESULT_MAX_CHARS)));
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}
