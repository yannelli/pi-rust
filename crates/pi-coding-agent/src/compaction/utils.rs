//! Shared by compaction and branch summaries: file operation tracking and conversation
//! serialization. Port of upstream `compaction/utils.ts`.

use std::collections::BTreeSet;

use pi_ai::{ContentBlock, JsonObject, Message, UserContent};
use serde_json::Value;

use crate::session::ids::js_len;

pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

/// Longest tool result text kept when serializing for a summary.
pub const TOOL_RESULT_MAX_CHARS: usize = 2000;

/// Paths touched by `read`, `write`, and `edit` tool calls.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileOperations {
    pub read: BTreeSet<String>,
    pub written: BTreeSet<String>,
    pub edited: BTreeSet<String>,
}

fn add_file_op(tool: &str, arguments: Option<&JsonObject>, ops: &mut FileOperations) {
    let path = arguments.and_then(|args| args.get("path")).and_then(Value::as_str);
    let Some(path) = path.filter(|path| !path.is_empty()) else { return };
    match tool {
        "read" => ops.read.insert(path.to_string()),
        "write" => ops.written.insert(path.to_string()),
        "edit" => ops.edited.insert(path.to_string()),
        _ => false,
    };
}

/// Record file operations from an assistant's tool calls, or from the nested calls a tool
/// result recorded (codemode scripts).
pub fn extract_file_ops_from_message(message: &Message, ops: &mut FileOperations) {
    match message {
        Message::ToolResult(result) => {
            for call in result.nested_calls.iter().flat_map(|nested| &nested.calls) {
                add_file_op(&call.name, call.arguments.as_ref(), ops);
            }
        }
        Message::Assistant(assistant) => {
            for call in assistant.tool_calls() {
                add_file_op(&call.name, Some(&call.arguments), ops);
            }
        }
        _ => {}
    }
}

/// Sorted files only read, and sorted files written or edited.
pub fn compute_file_lists(ops: &FileOperations) -> (Vec<String>, Vec<String>) {
    let changed: BTreeSet<&String> = ops.edited.iter().chain(&ops.written).collect();
    let read_only = ops.read.iter().filter(|path| !changed.contains(path)).cloned().collect();
    (read_only, changed.into_iter().cloned().collect())
}

/// The file list sections appended to a summary.
pub fn format_file_operations(read_files: &[String], modified_files: &[String]) -> String {
    let mut sections = Vec::new();
    if !read_files.is_empty() {
        sections.push(format!("<read-files>\n{}\n</read-files>", read_files.join("\n")));
    }
    if !modified_files.is_empty() {
        sections.push(format!("<modified-files>\n{}\n</modified-files>", modified_files.join("\n")));
    }
    if sections.is_empty() { String::new() } else { format!("\n\n{}", sections.join("\n\n")) }
}

/// Text blocks joined by `separator`, like upstream `contentText`.
pub fn blocks_text(blocks: &[ContentBlock], separator: &str) -> String {
    blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join(separator)
}

pub fn user_content_text(content: &UserContent, separator: &str) -> String {
    match content {
        UserContent::Text(text) => text.clone(),
        UserContent::Blocks(blocks) => blocks_text(blocks, separator),
    }
}

/// Keep the first `max` UTF-16 units of `text` and note how many were cut.
fn truncate_for_summary(text: &str, max: usize) -> String {
    let length = js_len(text);
    if length <= max {
        return text.to_string();
    }
    let (mut units, mut end) = (0, 0);
    for (index, ch) in text.char_indices() {
        if units + ch.len_utf16() > max {
            break;
        }
        units += ch.len_utf16();
        end = index + ch.len_utf8();
    }
    format!("{}\n\n[... {} more characters truncated]", &text[..end], length - max)
}

fn tool_call_text(call: &pi_ai::ToolCall) -> String {
    let args: Vec<String> = call
        .arguments
        .iter()
        .map(|(key, value)| format!("{key}={}", serde_json::to_string(value).unwrap_or_default()))
        .collect();
    format!("{}({})", call.name, args.join(", "))
}

/// Render provider messages as labeled text so the summarizer does not continue the
/// conversation. Run `convert_to_llm` first for custom roles.
pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message {
            Message::User(user) => {
                let text = user_content_text(&user.content, "");
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Message::Assistant(assistant) => {
                let mut thinking = Vec::new();
                let mut calls = Vec::new();
                for block in &assistant.content {
                    match block {
                        ContentBlock::Thinking { thinking: text, .. } => thinking.push(text.as_str()),
                        ContentBlock::ToolCall(call) => calls.push(tool_call_text(call)),
                        _ => {}
                    }
                }
                if !thinking.is_empty() {
                    parts.push(format!("[Assistant thinking]: {}", thinking.join("\n")));
                }
                if assistant.content.iter().any(|block| matches!(block, ContentBlock::Text { .. })) {
                    parts.push(format!("[Assistant]: {}", blocks_text(&assistant.content, "\n")));
                }
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Message::ToolResult(result) => {
                let text = blocks_text(&result.content, "");
                if !text.is_empty() {
                    parts.push(format!("[Tool result]: {}", truncate_for_summary(&text, TOOL_RESULT_MAX_CHARS)));
                }
            }
            Message::System(_) | Message::Custom(_) => {}
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use pi_ai::{NestedToolCallRecord, NestedToolCalls, ToolResultMessage};
    use serde_json::json;

    use super::*;

    fn tool_result(text: &str) -> Message {
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "tc1".into(),
            tool_name: "read".into(),
            content: vec![ContentBlock::text(text)],
            ..Default::default()
        })
    }

    #[test]
    fn truncates_long_tool_results_only() {
        let result = serialize_conversation(&[tool_result(&"x".repeat(5000))]);
        assert!(result.starts_with("[Tool result]:") && result.contains("[... 3000 more characters truncated]"));
        assert!(result.contains(&"x".repeat(2000)) && !result.contains(&"x".repeat(2001)));
        let short = "x".repeat(1500);
        assert_eq!(serialize_conversation(&[tool_result(&short)]), format!("[Tool result]: {short}"));
        let long = "y".repeat(5000);
        let user = Message::User(pi_ai::UserMessage {
            content: UserContent::Blocks(vec![ContentBlock::text(long.clone())]),
            timestamp: 0,
        });
        let text = serialize_conversation(&[user]);
        assert!(!text.contains("truncated") && text.contains(&long));
    }

    #[test]
    fn tracks_files_from_nested_calls() {
        let record = |id: &str, name: &str, arguments: Option<Value>| NestedToolCallRecord {
            id: id.into(),
            name: name.into(),
            arguments: arguments.and_then(|value| value.as_object().cloned()),
            arguments_bytes: None,
            status: "ok".into(),
            duration_ms: None,
            error: None,
        };
        let result = Message::ToolResult(ToolResultMessage {
            tool_call_id: "codemode-1".into(),
            tool_name: "codemode".into(),
            nested_calls: Some(NestedToolCalls {
                calls: vec![
                    record("codemode-1/1", "read", Some(json!({"path": "a.ts"}))),
                    record("codemode-1/2", "edit", Some(json!({"path": "b.ts", "edits": []}))),
                    record("codemode-1/3", "write", None),
                ],
                complete: false,
            }),
            ..Default::default()
        });
        let mut ops = FileOperations::default();
        extract_file_ops_from_message(&result, &mut ops);
        assert_eq!(compute_file_lists(&ops), (vec!["a.ts".to_string()], vec!["b.ts".to_string()]));
        assert_eq!(
            format_file_operations(&["a.ts".into()], &["b.ts".into()]),
            "\n\n<read-files>\na.ts\n</read-files>\n\n<modified-files>\nb.ts\n</modified-files>"
        );
    }
}
