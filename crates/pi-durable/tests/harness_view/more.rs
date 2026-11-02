//! The remaining upstream `harness-view.test.ts` cases.

use std::sync::LazyLock;

use pi_chord::{AbortController, BACKGROUND_CONTEXT, with_abort_signal};
use pi_durable::documents::*;
use pi_durable::session::WatchEnd;
use pi_durable::{ConversationOwnership, DraftHead};
use serde::{Deserialize, Serialize};

use super::*;
use crate::support::{ValueCall, on_value};

fn ownerless() -> CreateOptions {
    CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() }
}

async fn note(conversation: &Conversation, kind: &'static str, head: Option<EntryId>) -> pi_durable::EntryRecord {
    let id = conversation.id;
    let draft = EntryDraft { head: head.map(DraftHead::Entry), ..EntryDraft::new(kind) };
    conversation.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap()
}

fn kinds(view: &Json) -> Vec<String> {
    let entries = view.get("entries").unwrap().to_serde();
    entries.as_array().unwrap().iter().map(|entry| entry["kind"].as_str().unwrap().to_owned()).collect()
}

fn entry_ids(view: &Json) -> Vec<Value> {
    let entries = view.get("entries").unwrap().to_serde();
    entries.as_array().unwrap().iter().map(|entry| entry["id"].clone()).collect()
}

async fn open() -> (Harness, Conversation) {
    open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await
}

#[tokio::test]
async fn shows_a_forks_inherited_entries_in_id_order_its_head_markers_in_place() {
    let (harness, root) = open().await;
    let a = note(&root, "a", None).await;
    let b = note(&root, "b", None).await;
    let fork = root.fork(b.id, ownerless(), &context()).await.unwrap();
    let (acquired, frames, watch) = record(&fork).await;
    note(&fork, "summary", Some(b.id)).await;
    drained().await;
    assert_eq!(entry_ids(&acquired), [json!(a.id), json!(b.id)]);
    let frames = frames.lock().unwrap().clone();
    assert_eq!(kinds(&frames[0].0), ["a", "b", "summary"]);
    let expected = committed(&harness, &fork, acquired.get("conversation").unwrap(), 3).await;
    assert_eq!(frames[0].0, Json::from(expected));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn shows_a_forks_inherited_entries_and_follows_only_the_forks_own_commits() {
    let (harness, root) = open().await;
    let first = note(&root, "first", None).await;
    let fork = root.fork(first.id, ownerless(), &context()).await.unwrap();
    let (acquired, frames, watch) = record(&fork).await;
    assert_eq!(kinds(&acquired), ["first"]);
    let parent = acquired.get("conversation").unwrap().to_serde()["parent"].clone();
    assert_eq!(parent, json!({ "conversationId": root.id, "at": first.id }));
    note(&root, "parent", None).await;
    note(&fork, "child", None).await;
    drained().await;
    let seen: Vec<Vec<String>> = frames.lock().unwrap().iter().map(|(value, _)| kinds(value)).collect();
    assert_eq!(seen, [vec!["first", "child"]]);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn unmounts_a_retired_document_and_mounts_its_recreation_whole() {
    let (harness, root) = open().await;
    let (_, frames, watch) = record(&root).await;
    let id = root.id;
    root.commit(move |tx: Tx| async move { tx.retire_doc(&LIVE_DOC, id).await }, &context()).await.unwrap();
    let recreate = move |tx: Tx| async move { tx.doc(&LIVE_DOC, id).await?.edit(|live| live.tools = Some(Vec::new())) };
    root.commit(recreate, &context()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let ops: Vec<Vec<Value>> = frames.iter().map(|(_, ops)| ops_json(ops)).collect();
    assert_eq!(
        ops,
        [vec![json!(["d", ["docs", "pi.live"]])], vec![json!(["s", ["docs", "pi.live"], { "tools": [] }])]]
    );
    assert!(frames[0].0.get("docs").unwrap().get("pi.live").is_none());
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn folds_undelivered_commits_into_one_call_whose_ops_reach_the_newest_view() {
    let (harness, root) = open().await;
    let watch = root.watch(&context(), None).await.unwrap();
    let initial = watch.value();
    for _ in 0..101 {
        note(&root, "note", None).await;
    }
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(on_value(move |ValueCall { value, ops, .. }, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        }))
        .unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert!(!frames[0].1[0].is_replace());
    assert_eq!(pi_chord::delta::apply_immutable(Some(&initial), &frames[0].1).unwrap(), frames[0].0);
    // The default window took 100 appends, then dropped its oldest 51 at the 101st, down to 50.
    let expected = committed(&harness, &root, initial.get("conversation").unwrap(), 50).await;
    assert_eq!(frames[0].0, Json::from(expected));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_states_and_watches_of_one_conversation_independent_and_remounts_after_the_last_one_detaches() {
    let (harness, root) = open().await;
    let state = root.view_state(&context()).await.unwrap();
    let (_, frames, watch) = record(&root).await;
    note(&root, "one", None).await;
    drained().await;
    assert_eq!(kinds(&state.value()), ["one"]);
    watch.stop().await;
    assert_eq!(frames.lock().unwrap().len(), 1);
    note(&root, "two", None).await;
    drained().await;
    assert_eq!(kinds(&state.value()), ["one", "two"]);
    let last = state.value();
    state.dispose().unwrap();
    // No observer is left, so the mount was dropped: a new observer builds a new revision from committed state.
    let rebuilt = fresh(&root).await;
    assert_eq!(rebuilt, last);
    assert!(!rebuilt.same_ref(&last));
    harness.close(&context()).await.unwrap();
}

/// Hold the Session line with a commit that waits for the returned gate.
async fn hold(root: &Conversation) -> (Deferred, tokio::task::JoinHandle<()>) {
    let release = Deferred::default();
    let (gate, root) = (release.clone(), root.clone());
    let blocking = tokio::spawn(async move {
        let held = move |_: Tx| async move {
            gate.wait().await;
            Ok(())
        };
        root.commit(held, &context()).await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    (release, blocking)
}

#[tokio::test]
async fn rejects_an_acquisition_cancelled_or_closed_while_it_waits_for_the_session_line() {
    let (harness, root) = open().await;
    let (release, blocking) = hold(&root).await;
    let controller = AbortController::new();
    let cancellable = with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT);
    let waiting = root.clone();
    let cancelled = tokio::spawn(async move { waiting.watch(&cancellable, None).await.map(|_| ()) });
    tokio::task::yield_now().await;
    controller.abort(Some(pi_chord::ChordError::error("cancelled")));
    release.resolve();
    blocking.await.unwrap();
    let error = cancelled.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");

    let (release, blocking) = hold(&root).await;
    let waiting = root.clone();
    let queued = tokio::spawn(async move { waiting.watch(&context(), None).await.map(|_| ()) });
    tokio::task::yield_now().await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    tokio::task::yield_now().await;
    release.resolve();
    blocking.await.unwrap();
    let error = queued.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("Harness is closed"), "{error}");
    closing.await.unwrap().unwrap();
}

#[tokio::test]
async fn shares_one_mount_between_concurrent_observers_and_isolates_a_failing_listener() {
    let (harness, root) = open().await;
    let failing = root.watch(&context(), None).await.unwrap();
    let (_, frames, watch) = record(&root).await;
    let shared = root.view_state(&context()).await.unwrap();
    // Each observer has its own window; the documents are the mount's.
    assert_eq!(failing.value(), shared.value());
    assert!(failing.value().get("docs").unwrap().same_ref(shared.value().get("docs").unwrap()));
    shared.dispose().unwrap();
    failing
        .start(on_value(|ValueCall { .. }, _| async { Err(pi_durable::DurableError::error("listener failed")) }))
        .unwrap();
    note(&root, "one", None).await;
    note(&root, "two", None).await;
    drained().await;
    assert!(matches!(failing.closed().await, WatchEnd::ListenerError(_)));
    assert_eq!(frames.lock().unwrap().len(), 2);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Other {
    n: u32,
}

static OTHER_DOC: LazyLock<DocToken<Other, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("app.other", 1, LatestConversation(LatestFork::Initial), Other::default, |_, _, _| {
        Ok(false)
    }))
    .unwrap()
});

#[tokio::test]
async fn publishes_one_frame_for_a_commit_that_appends_several_entries_and_edits_a_document() {
    let (harness, root) = open().await;
    let (acquired, frames, watch) = record(&root).await;
    let id = root.id;
    let change = move |tx: Tx| async move {
        tx.append_entry(id, EntryDraft::new("a")).await?;
        tx.doc(&LIVE_DOC, id).await?.edit(|live| live.tools = Some(Vec::new()))?;
        tx.append_entry(id, EntryDraft::new("b")).await?;
        Ok(())
    };
    root.commit(change, &context()).await.unwrap();
    // A document that is not mounted publishes nothing.
    let other = move |tx: Tx| async move { tx.doc(&OTHER_DOC, id).await?.edit(|other| other.n = 1) };
    root.commit(other, &context()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(kinds(&frames[0].0), ["a", "b"]);
    let expected = committed(&harness, &root, acquired.get("conversation").unwrap(), 2).await;
    assert_eq!(Json::from(replay(&acquired, &frames)), Json::from(expected));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}
