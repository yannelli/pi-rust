//! Helpers shared by the session and compaction tests, after upstream `test/utilities.ts`.
#![allow(dead_code)]

use std::path::Path;

use pi_ai::{AssistantMessage, ContentBlock, Message, StopReason, Usage, UserContent, UserMessage};
use pi_coding_agent::session::{Entry, FileEntry, NewSessionOptions, SessionManager};
use serde_json::{Value, json};

pub fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: pi_ai::now_ms() })
}

pub fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        ..Default::default()
    }
}

pub fn assistant_with(text: &str, usage: Usage) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![ContentBlock::text(text)],
        api: "anthropic-messages".into(),
        provider: "anthropic".into(),
        model: "test".into(),
        usage,
        stop_reason: StopReason::Stop,
        timestamp: pi_ai::now_ms(),
        ..Default::default()
    })
}

pub fn assistant(text: &str) -> Message {
    assistant_with(text, usage(1, 1, 0, 0))
}

pub fn text(message: &Message) -> String {
    match message {
        Message::User(user) => user.content.text(),
        Message::Assistant(assistant) => assistant.text(),
        Message::ToolResult(result) => result.content.iter().filter_map(ContentBlock::as_text).collect(),
        Message::Custom(custom) => custom
            .str_field("summary")
            .map(String::from)
            .or_else(|| custom.str_field("content").map(String::from))
            .unwrap_or_default(),
        Message::System(system) => system.content.text(),
    }
}

pub fn roles(messages: &[Message]) -> Vec<String> {
    messages.iter().map(|message| message.role().to_string()).collect()
}

pub fn memory() -> SessionManager {
    SessionManager::in_memory(Path::new("/project"))
}

/// A session over hand-built entries (no header), like upstream `buildSessionContext(entries)`.
pub fn with_entries(entries: Vec<Value>) -> SessionManager {
    let raw: Vec<FileEntry> = entries.into_iter().map(|entry| entry.as_object().cloned().unwrap()).collect();
    SessionManager::in_memory_with(Path::new("/project"), NewSessionOptions::default(), raw).unwrap()
}

pub fn raw_entries(session: &SessionManager) -> Vec<FileEntry> {
    session.entries().iter().map(|entry| entry.raw().clone()).collect()
}

pub fn ids(entries: &[&Entry]) -> Vec<String> {
    entries.iter().map(|entry| entry.id().to_string()).collect()
}

pub fn message_entry(id: &str, parent: Option<&str>, message: &Message) -> Value {
    json!({"type": "message", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z", "message": message})
}

pub fn compaction_entry(id: &str, parent: Option<&str>, summary: &str, first_kept: &str) -> Value {
    json!({"type": "compaction", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z",
        "summary": summary, "firstKeptEntryId": first_kept, "tokensBefore": 1000})
}

/// One label per line: the message role for message entries, else the entry type.
pub fn file_roles(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| {
            let record: Value = serde_json::from_str(line).unwrap();
            record["message"]["role"].as_str().or(record["type"].as_str()).unwrap_or_default().to_string()
        })
        .collect()
}
