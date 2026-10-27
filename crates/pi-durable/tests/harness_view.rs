//! Port of upstream `test/harness-view.test.ts` .

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::ThinkingLevel;
use pi_chord::delta::{Op, apply_immutable};
use pi_chord::{Json, JsonMap};
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::{PROVIDER_DOC, USAGE_DOC};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message};
use pi_durable::{EntryDraft, EntryId};
use serde_json::{Value, json};
use support::harness_support::*;
use support::{Deferred, context};

const MOUNTED: [&str; 5] = ["pi.agent", "pi.inbox", "pi.live", "pi.provider", "pi.usage"];

type Frames = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

/// Start a watch of `conversation` that records its acquisition revision and every delivered frame.
async fn record(conversation: &Conversation) -> (Json, Frames, ConversationWatch) {
    let watch = conversation.watch(&context()).await.unwrap();
    let acquired = watch.value();
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(move |pi_chord::WatchChange { value, ops, .. }, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        })
        .unwrap();
    (acquired, frames, watch)
}

async fn fresh(conversation: &Conversation) -> Json {
    let state = conversation.view_state(&context()).await.unwrap();
    let value = state.value();
    state.dispose().unwrap();
    value
}

/// The view as committed state defines it, read without any mount.
async fn committed(harness: &Harness, conversation: &Conversation, record: &Json) -> Value {
    let mut docs = JsonMap::new();
    let id = conversation.id;
    let values = [
        ("pi.agent", harness.snapshot(&AGENT_DOC, id, &context()).await.unwrap()),
        ("pi.live", harness.snapshot(&LIVE_DOC, id, &context()).await.unwrap()),
        ("pi.inbox", harness.snapshot(&INBOX_DOC, id, &context()).await.unwrap()),
        ("pi.provider", harness.snapshot(&PROVIDER_DOC, id, &context()).await.unwrap()),
        ("pi.usage", harness.snapshot(&USAGE_DOC, id, &context()).await.unwrap()),
    ];
    for (kind, value) in values {
        if let Some(value) = value {
            docs.insert(kind, value);
        }
    }
    let entries = conversation.context(&context(), None).await.unwrap().entries;
    json!({ "conversation": record.to_serde(), "entries": serde_json::to_value(entries).unwrap(), "docs": Json::object(docs).to_serde() })
}

/// Apply every frame's operations to the acquisition value, checking each delivered revision on the way.
fn replay(acquired: &Json, frames: &[(Json, Vec<Op>)]) -> Value {
    let mut value = acquired.clone();
    for (delivered, ops) in frames {
        value = apply_immutable(Some(&value), ops).unwrap();
        assert_eq!(value.to_serde(), delivered.to_serde());
    }
    value.to_serde()
}

fn ops_json(ops: &[Op]) -> Vec<Value> {
    ops.iter().map(|op| serde_json::to_value(op).unwrap()).collect()
}

#[path = "harness_view/more.rs"]
mod more;

async fn drained() {
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
}

#[tokio::test]
async fn hydrates_the_active_entries_and_the_built_in_documents() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    setup.faux.set_responses([faux_assistant_message("hello", FauxMessageOptions::default())]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let view = fresh(&root).await.to_serde();
    assert_eq!(view["conversation"], json!({ "id": root.id }));
    // Numbers compare by value: a stored `0` and a serialized `0.0` are equal.
    assert_eq!(
        Json::from(view["entries"].clone()),
        Json::from(serde_json::to_value(all_entries(&root).await).unwrap())
    );
    let mut keys: Vec<&str> = view["docs"].as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, MOUNTED);
    assert_eq!(view["docs"]["pi.live"], json!({}));
    assert_eq!(view["docs"]["pi.inbox"], json!({ "items": [] }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn publishes_frames_whose_operations_rebuild_every_revision() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let release = Deferred::default();
    let held = release.clone();
    setup.faux.set_responses([pi_durable::testing::faux::FauxResponseStep::async_factory(move |_, _, _, _| {
        let held = held.clone();
        Box::pin(async move {
            held.wait().await;
            Ok(faux_assistant_message("a longer answer", FauxMessageOptions::default()))
        })
    })]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (acquired, frames, watch) = record(&root).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    wait_for(|| {
        let frames = frames.clone();
        async move {
            let frames = frames.lock().unwrap();
            frames
                .iter()
                .any(|(value, _)| value.get("docs").and_then(|d| d.get("pi.live")?.get("generation")).is_some())
        }
    })
    .await;
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let expected = committed(&harness, &root, acquired.get("conversation").unwrap()).await;
    assert_eq!(Json::from(replay(&acquired, &frames)), Json::from(expected));
    let first = ops_json(&frames[0].1);
    assert!(
        first.iter().any(|op| op[0] == "p" && op[1] == json!(["entries"]) && op[4][0]["kind"] == "pi.user"),
        "{first:?}"
    );
    // Document operations keep their exact shape under the mount path.
    let all: Vec<Value> = frames.iter().flat_map(|(_, ops)| ops_json(ops)).collect();
    assert!(all.contains(&json!(["s", ["docs", "pi.live", "generation"], { "attempt": 1 }])), "{all:?}");
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn skips_commits_that_touch_nothing_mounted() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let other = harness.create_conversation(CreateOptions::default(), &context()).await.unwrap();
    let (acquired, frames, watch) = record(&root).await;
    let other_id = other.id;
    let note = move |tx: Tx| async move { tx.append_entry(other_id, EntryDraft::new("note")).await };
    other.commit(note, &context()).await.unwrap();
    let change = AgentChange { thinking_level: Change::Set(ThinkingLevel::High), ..AgentChange::default() };
    root.configure(change, &context()).await.unwrap();
    let root_id = root.id;
    let note = move |tx: Tx| async move { tx.append_entry(root_id, EntryDraft::new("note")).await };
    root.commit(note, &context()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 2);
    assert_eq!(ops_json(&frames[0].1), [json!(["s", ["docs", "pi.agent", "thinkingLevel"], "high"])]);
    let part =
        |value: &Json, path: &[&str]| path.iter().fold(value.clone(), |value, key| value.get(key).unwrap().clone());
    assert!(part(&frames[0].0, &["entries"]).same_ref(&part(&acquired, &["entries"])));
    assert!(part(&frames[0].0, &["docs", "pi.live"]).same_ref(&part(&acquired, &["docs", "pi.live"])));
    assert!(part(&frames[1].0, &["docs"]).same_ref(&part(&frames[0].0, &["docs"])));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cuts_the_entries_at_a_head_marker_keeping_the_entries_from_its_head() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let id = root.id;
    let note = |kind: &'static str, head: Option<EntryId>| {
        let root = root.clone();
        async move {
            let draft = EntryDraft { head: head.map(pi_durable::DraftHead::Entry), ..EntryDraft::new(kind) };
            root.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap()
        }
    };
    note("a", None).await;
    let b = note("b", None).await;
    note("c", None).await;
    let (acquired, frames, watch) = record(&root).await;
    let summary = note("summary", Some(b.id)).await;
    note("d", None).await;
    root.reset(None, &context()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let kinds: Vec<Vec<String>> = frames
        .iter()
        .map(|(value, _)| {
            let entries = value.get("entries").unwrap().to_serde();
            entries.as_array().unwrap().iter().map(|entry| entry["kind"].as_str().unwrap().to_owned()).collect()
        })
        .collect();
    assert_eq!(kinds, [vec!["summary", "b", "c"], vec!["summary", "b", "c", "d"], vec!["pi.reset"]]);
    let summary = serde_json::to_value(&summary).unwrap();
    assert_eq!(ops_json(&frames[0].1), [json!(["p", ["entries"], 0, 1, [summary]])]);
    let expected = committed(&harness, &root, acquired.get("conversation").unwrap()).await;
    assert_eq!(Json::from(replay(&acquired, &frames)), Json::from(expected));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn ends_states_and_watches_at_close_and_rejects_later_acquisition() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    let (_, _, watch) = record(&root).await;
    harness.close(&context()).await.unwrap();
    watch.closed().await;
    assert!(root.watch(&context()).await.is_err());
    assert!(root.view_state(&context()).await.is_err());
}
