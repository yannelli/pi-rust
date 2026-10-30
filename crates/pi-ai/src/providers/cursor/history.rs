//! pi's transcript as Cursor history: JSON prompt records, and structured turns of
//! `ConversationStep`s with their MCP tool calls and results.

use std::collections::{HashMap, HashSet};

use serde_json::{Value as Json, json};

use super::proto::Writer;
use super::request::{Blobs, MCP_PROVIDER, deterministic_uuid, sanitize_call_id};
use super::value;
use crate::types::{AssistantMessage, ContentBlock, Message, ToolResultMessage, UserContent};

/// Tool results by sanitized call id; a repeated id keeps its last result.
pub(crate) type Results<'a> = HashMap<String, &'a ToolResultMessage>;

pub(crate) fn user_text(content: &UserContent) -> String {
    match content {
        UserContent::Text(text) => text.trim().to_string(),
        UserContent::Blocks(blocks) => {
            let texts = blocks.iter().filter_map(ContentBlock::as_text).map(str::trim);
            texts.filter(|text| !text.is_empty()).collect::<Vec<_>>().join("\n")
        }
    }
}

pub(crate) fn user_images(content: &UserContent) -> Vec<(&str, &str)> {
    let UserContent::Blocks(blocks) = content else { return Vec::new() };
    let images = blocks.iter().filter_map(|block| match block {
        ContentBlock::Image { data, mime_type, .. } => Some((mime_type.as_str(), data.as_str())),
        _ => None,
    });
    images.collect()
}

fn user_record(content: &UserContent) -> Option<Json> {
    let text = user_text(content);
    let mut parts: Vec<Json> = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    for (mime, data) in user_images(content) {
        parts.push(json!({"type": "image", "image": format!("data:{mime};base64,{data}"), "mediaType": mime}));
    }
    (!parts.is_empty()).then(|| json!({"role": "user", "content": parts}))
}

fn result_text(result: &ToolResultMessage) -> String {
    let parts = result.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(text.clone()),
        ContentBlock::Image { mime_type, .. } => Some(format!("[{mime_type} image]")),
        _ => None,
    });
    parts.collect::<Vec<_>>().join("\n")
}

fn tool_record(result: &ToolResultMessage) -> Json {
    let id = sanitize_call_id(&result.tool_call_id);
    let text = result_text(result);
    let mut part = json!({"type": "tool-result", "toolName": result.tool_name, "toolCallId": id, "result": text});
    if result.is_error {
        part["isError"] = json!(true);
    }
    json!({"role": "tool", "id": id, "content": [part]})
}

fn orphan_text(result: &ToolResultMessage) -> String {
    let label = if result.is_error { "[Tool Error]" } else { "[Tool Result]" };
    format!("{label}\n{}", result_text(result))
}

/// Assistant blocks split where text follows a tool call; thinking is not replayed.
fn rounds(assistant: &AssistantMessage) -> Vec<Vec<&ContentBlock>> {
    let (mut rounds, mut current, mut after_call) = (Vec::new(), Vec::new(), false);
    for block in &assistant.content {
        match block {
            ContentBlock::ToolCall(_) => {
                current.push(block);
                after_call = true;
            }
            ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                if after_call {
                    rounds.push(std::mem::take(&mut current));
                    after_call = false;
                }
                current.push(block);
            }
            _ => {}
        }
    }
    if !current.is_empty() {
        rounds.push(current);
    }
    rounds
}

fn call_id(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::ToolCall(call) => Some(sanitize_call_id(&call.id)),
        _ => None,
    }
}

fn assistant_record(round: &[&ContentBlock], seen: &mut HashSet<String>) -> Json {
    let parts = round.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
        ContentBlock::ToolCall(call) => {
            let id = sanitize_call_id(&call.id);
            let args = Json::Object(call.arguments.clone());
            seen.insert(id.clone())
                .then(|| json!({"type": "tool-call", "toolCallId": id, "toolName": call.name, "args": args}))
        }
        _ => None,
    });
    json!({"role": "assistant", "content": parts.collect::<Vec<_>>()})
}

/// The JSON prompt records of `history`. Results of a round that text follows come right after it.
pub(crate) fn records(history: &[Message], results: &Results) -> Vec<Json> {
    let (mut records, mut known, mut emitted) = (Vec::new(), HashSet::new(), HashSet::new());
    for message in history {
        match message {
            Message::User(user) => records.extend(user_record(&user.content)),
            Message::Assistant(assistant) => {
                let rounds = rounds(assistant);
                let mut seen = HashSet::new();
                for (index, round) in rounds.iter().enumerate() {
                    records.push(assistant_record(round, &mut seen));
                    for id in round.iter().filter_map(|block| call_id(block)) {
                        known.insert(id.clone());
                        if index + 1 < rounds.len()
                            && let Some(result) = results.get(&id)
                            && emitted.insert(id)
                        {
                            records.push(tool_record(result));
                        }
                    }
                }
            }
            Message::ToolResult(result) => {
                let id = sanitize_call_id(&result.tool_call_id);
                if !known.contains(&id) {
                    records
                        .push(json!({"role": "assistant", "content": [{"type": "text", "text": orphan_text(result)}]}));
                } else if emitted.insert(id.clone()) {
                    records.push(tool_record(results.get(&id).copied().unwrap_or(result)));
                }
            }
            _ => {}
        }
    }
    records
}

fn mcp_result(result: &ToolResultMessage) -> Writer {
    if result.is_error {
        return Writer::new().message(2, Writer::new().string(1, &result_text(result)));
    }
    let items = result.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(Writer::new().message(1, Writer::new().string(1, text))),
        ContentBlock::Image { data, mime_type, .. } => {
            let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).ok()?;
            Some(Writer::new().message(2, Writer::new().bytes(1, &bytes).string(2, mime_type)))
        }
        _ => None,
    });
    Writer::new().message(1, items.fold(Writer::new(), |success, item| success.message(1, item)))
}

fn tool_step(call: &crate::types::ToolCall, results: &Results) -> Writer {
    let id = sanitize_call_id(&call.id);
    let args = Writer::new().string(1, &call.name).string(3, &id).string(4, MCP_PROVIDER).string(5, &call.name);
    let args = call.arguments.iter().fold(args, |args, (key, value)| {
        args.message(2, Writer::new().string(1, key).message(2, value::encode(value)))
    });
    let mut mcp = Writer::new().message(1, args);
    if let Some(result) = results.get(&id) {
        mcp = mcp.message(2, mcp_result(result));
    }
    Writer::new().message(2, Writer::new().message(15, mcp).string(57, &id))
}

/// `ConversationTurnStructure` blob ids, one per user message of `history`.
pub(crate) fn turns(history: &[Message], results: &Results, blobs: &mut Blobs) -> Vec<Vec<u8>> {
    let mut turns: Vec<(Vec<u8>, Vec<Vec<u8>>)> = Vec::new();
    let mut known = HashSet::new();
    for message in history {
        match message {
            Message::User(user) => {
                let text = user_text(&user.content);
                let id = deterministic_uuid(&format!("u:{}:{text}", turns.len()));
                turns.push((blobs.put(Writer::new().string(1, &text).string(2, &id).finish()), Vec::new()));
            }
            Message::Assistant(assistant) => {
                let Some((_, steps)) = turns.last_mut() else { continue };
                for block in &assistant.content {
                    let step = match block {
                        ContentBlock::Text { text, .. } if !text.trim().is_empty() => {
                            Writer::new().message(1, Writer::new().string(1, text))
                        }
                        ContentBlock::ToolCall(call) => {
                            known.insert(sanitize_call_id(&call.id));
                            tool_step(call, results)
                        }
                        _ => continue,
                    };
                    steps.push(blobs.put(step.finish()));
                }
            }
            Message::ToolResult(result) if !known.contains(&sanitize_call_id(&result.tool_call_id)) => {
                if let Some((_, steps)) = turns.last_mut() {
                    let step = Writer::new().message(1, Writer::new().string(1, &orphan_text(result)));
                    steps.push(blobs.put(step.finish()));
                }
            }
            _ => {}
        }
    }
    let turns = turns.into_iter().map(|(user, steps)| {
        let structure = steps.iter().fold(Writer::new().bytes(1, &user), |turn, step| turn.bytes(2, step));
        blobs.put(Writer::new().message(1, structure).finish())
    });
    turns.collect()
}
