//! Port of the runnable examples in upstream `test/examples/`. Each script becomes one test that checks what the
//! script prints. The modules under `examples/` group them by topic.

#[path = "examples/agents.rs"]
mod agents;
#[path = "examples/chat.rs"]
mod chat;
#[path = "examples/coding.rs"]
mod coding;
#[path = "examples/compaction.rs"]
mod compaction;
#[path = "examples/harness.rs"]
mod harness;
#[path = "examples/session.rs"]
mod session;
#[path = "examples/subagents.rs"]
mod subagents;
mod support;
#[path = "examples/tools.rs"]
mod tools;

use std::sync::{Arc, LazyLock};

use pi_chord::{Context, Json};
use pi_durable::documents::*;
use pi_durable::harness::*;
use pi_durable::session::{Session, SessionOptions, Tx, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use support::context;
use support::harness_support::*;

fn ctx() -> Context {
    context()
}

fn memory() -> Arc<MemoryStorage> {
    Arc::new(MemoryStorage::new())
}

fn new_session() -> Session {
    create_session(memory(), SessionOptions::default())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Notes {
    text: String,
}

/// The `example.notes` document of most examples: rewindable, and a fork starts from its value at the fork entry.
static NOTES: LazyLock<DocToken<Notes, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("example.notes", 1, RewindableConversation(DocumentFork::AsOf), Notes::default)).unwrap()
});

fn notes(value: Option<Json>) -> Option<String> {
    Some(value?.get("text")?.as_str()?.to_owned())
}

async fn ownerless(session: &Session) -> ConversationRecord {
    let create = |tx: Tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await };
    session.commit(create, &ctx()).await.unwrap()
}

/// Append a `note` entry with `data`, and set the notes text in the same commit.
async fn note_with(session: &Session, id: ConversationId, data: &str, text: &str) -> EntryRecord {
    let (data, text) = (Json::from(json!(data)), text.to_owned());
    let write = move |tx: Tx| async move {
        let entry = tx.append_entry(id, EntryDraft { data: Some(data), ..EntryDraft::new("note") }).await?;
        tx.doc(&NOTES, id).await?.edit(|notes| notes.text = text)?;
        Ok(entry)
    };
    session.commit(write, &ctx()).await.unwrap()
}

/// A tool with a `path` parameter whose output is `<name> <path>`.
fn path_tool(name: &str, description: &str) -> ToolRegistration {
    let parameters = json!({ "type": "object", "properties": { "path": { "type": "string" } }, "required": ["path"] });
    let label = name.to_owned();
    define_tool(name, description, parameters, move |args, _, _| {
        let text = format!("{label} {}", args.get("path").and_then(Value::as_str).unwrap_or_default());
        async move { Ok(ToolExecutionResult::text(text)) }
    })
}

fn tool_names(agent: &Agent) -> Vec<String> {
    agent.tools.iter().map(|tool| tool.name().to_owned()).collect()
}
