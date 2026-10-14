//! Port of upstream `test/harness-context.test.ts`. The modules under `harness_context/` hold the derivation cases
//! with system messages and forks, and the kept context read cases.

#[path = "harness_context/derivation.rs"]
mod derivation;
#[path = "harness_context/reads.rs"]
mod reads;
#[path = "harness_context/steps.rs"]
mod steps;
mod support;

use std::sync::Arc;

use pi_ai::{ContentBlock, Message, StopReason, UserContent, UserMessage};
use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxMessageOptions, faux_assistant_message};
use pi_durable::{ContextEdit, ContextEditAction, DraftHead, EntryDraft, EntryId, EntryRecord};
use serde_json::json;
use support::context;
use support::harness_support::*;

fn user(text: &str) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp: 1 })
}

fn assistant(text: &str, stop_reason: StopReason) -> Message {
    Message::Assistant(faux_assistant_message(text, FauxMessageOptions::stop_reason(stop_reason)))
}

/// Compact message rendering for assertions (upstream `describeMessage`).
fn describe(message: &Message) -> String {
    let first_text = |content: &[ContentBlock]| content.iter().find_map(ContentBlock::as_text).unwrap_or("").to_owned();
    match message {
        Message::User(_) => format!("user:{}", text_of(Some(message)).unwrap_or_default()),
        Message::Assistant(assistant) => format!("assistant:{}", first_text(&assistant.content)),
        Message::ToolResult(result) if result.is_error => format!("result:{}:error", result.tool_call_id),
        Message::ToolResult(result) => format!("result:{}:{}", result.tool_call_id, first_text(&result.content)),
        Message::System(system) => {
            let keys: Vec<&str> =
                system.sections.iter().flat_map(|sections| sections.keys()).map(String::as_str).collect();
            format!("system:{}", keys.join(","))
        }
        _ => "other".to_owned(),
    }
}

struct Setup {
    harness: Harness,
    root: Conversation,
}

impl Setup {
    async fn new() -> Self {
        let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
        Self { harness, root }
    }

    async fn append(&self, draft: EntryDraft) -> EntryRecord {
        let id = self.root.id;
        self.root.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap()
    }

    async fn message(&self, message: Message) -> EntryRecord {
        self.append(EntryDraft { model: Some(vec![message]), ..EntryDraft::new("message") }).await
    }

    async fn view(&self) -> ContextView {
        self.root.context(&context(), None).await.unwrap()
    }
}

fn ids(entries: &[EntryRecord]) -> Vec<EntryId> {
    entries.iter().map(|entry| entry.id).collect()
}

fn described(view: &ContextView) -> Vec<String> {
    view.messages.iter().map(describe).collect()
}

#[tokio::test]
async fn returns_the_whole_transcript_without_a_head_and_excludes_model_less_entries_from_messages() {
    let setup = Setup::new().await;
    let first = setup.message(user("hi")).await;
    let note = setup
        .append(EntryDraft { data: Some(Json::from(json!({ "text": "display only" }))), ..EntryDraft::new("note") })
        .await;
    let answer = setup.message(assistant("hello", StopReason::Stop)).await;
    let view = setup.view().await;
    assert!(view.head.is_none());
    assert_eq!(ids(&view.entries), [first.id, note.id, answer.id]);
    assert_eq!(described(&view), ["user:hi", "assistant:hello"]);
    setup.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn excludes_aborted_error_and_deferred_assistant_messages_but_keeps_their_raw_entries() {
    let setup = Setup::new().await;
    setup.message(user("q")).await;
    let aborted = setup.message(assistant("partial", StopReason::Aborted)).await;
    setup.message(assistant("failed", StopReason::Error)).await;
    setup.message(assistant("later", StopReason::Deferred)).await;
    setup.message(assistant("done", StopReason::Length)).await;
    let view = setup.view().await;
    assert_eq!(view.entries.len(), 5);
    assert_eq!(view.entries[1].id, aborted.id);
    assert_eq!(described(&view), ["user:q", "assistant:done"]);
    setup.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn resolves_self_heads_and_uses_the_newest_head_marker() {
    let setup = Setup::new().await;
    setup.message(user("old")).await;
    let draft = EntryDraft {
        head: Some(DraftHead::SelfEntry),
        model: Some(vec![user("fresh start")]),
        ..EntryDraft::new("reset")
    };
    let reset = setup.append(draft).await;
    assert_eq!(reset.head, Some(reset.id));
    let after = setup.message(assistant("after reset", StopReason::Stop)).await;
    let view = setup.view().await;
    assert_eq!(view.head.as_ref().map(|head| head.id), Some(reset.id));
    assert_eq!(ids(&view.entries), [reset.id, after.id]);
    assert_eq!(described(&view), ["user:fresh start", "assistant:after reset"]);

    // A compaction summary heads an earlier kept entry; older head markers in range drop out.
    let draft = EntryDraft {
        head: Some(DraftHead::Entry(after.id)),
        model: Some(vec![user("summary")]),
        ..EntryDraft::new("summary")
    };
    let summary = setup.append(draft).await;
    let tail = setup.message(user("next")).await;
    let view = setup.view().await;
    assert_eq!(view.head.as_ref().map(|head| head.id), Some(summary.id));
    assert_eq!(ids(&view.entries), [summary.id, after.id, tail.id]);
    assert_eq!(described(&view), ["user:summary", "assistant:after reset", "user:next"]);
    setup.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn applies_the_newest_edit_per_target_within_the_active_range() {
    let setup = Setup::new().await;
    let first = setup.message(user("first")).await;
    let second = setup.message(user("second")).await;
    let edit = |edit: ContextEdit| EntryDraft { edits: Some(vec![edit]), ..EntryDraft::new("edit") };
    let replace = |target: EntryId, text: &str| ContextEdit {
        target,
        action: ContextEditAction::Replace,
        messages: Some(vec![user(text)]),
    };
    setup.append(edit(replace(first.id, "first v2"))).await;
    setup.append(edit(replace(first.id, "first v3"))).await;
    setup.append(edit(ContextEdit::omit(second.id))).await;
    let view = setup.view().await;
    assert_eq!(view.entries.len(), 5);
    assert_eq!(described(&view), ["user:first v3"]);

    // Edits before the active range no longer apply.
    let reset = setup.append(EntryDraft { head: Some(DraftHead::Entry(second.id)), ..EntryDraft::new("reset") }).await;
    let view = setup.view().await;
    assert_eq!(view.head.as_ref().map(|head| head.id), Some(reset.id));
    assert!(described(&view).is_empty());
    setup.append(edit(replace(second.id, "second v2"))).await;
    assert_eq!(described(&setup.view().await), ["user:second v2"]);
    setup.harness.close(&context()).await.unwrap();
}
