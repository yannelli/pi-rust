//! Port of upstream `test/session-definitions.test.ts`. Accepted overload forms compile and run here; each
//! `@ts-expect-error` form upstream is a Rust compile error, noted where upstream asserts it.

mod support;

use std::sync::{Arc, LazyLock};

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::entries::{TypedEntryDraft, define_entry};
use pi_durable::session::{Draft, Tx};
use pi_durable::tasks::{TaskOptions, TaskTemplate};
use pi_durable::*;
use serde::{Deserialize, Serialize};
use support::*;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct State {
    value: i64,
}

fn initial() -> State {
    State { value: 0 }
}

fn seeded(seed: i64) -> State {
    State { value: seed }
}

static SESSION_DOC: LazyLock<DocToken<State, SessionScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("t.session", 1, SessionScope, initial, |_, _, _| Ok(false))).unwrap());
static LATEST_DOC: LazyLock<DocToken<State, LatestConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("t.latest", 1, LatestConversation(LatestFork::Current), initial, |_, _, _| Ok(false)))
        .unwrap()
});
static REWINDABLE_DOC: LazyLock<DocToken<State, RewindableConversation>> = LazyLock::new(|| {
    define_doc(DocSpec::new("t.rewindable", 1, RewindableConversation(DocumentFork::AsOf), initial, |_, _, _| {
        Ok(false)
    }))
    .unwrap()
});
static TASK_DOC: LazyLock<DocToken<State, TaskScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("t.task", 1, TaskScope, initial, |_, _, _| Ok(false))).unwrap());
static SESSION_FAMILY: LazyLock<DocFamilyToken<State, i64, SessionScope>> = LazyLock::new(|| {
    define_doc_family(DocFamilySpec::new("t.session-family", 1, SessionScope, seeded, |_, _, _| Ok(false))).unwrap()
});
static CONVERSATION_FAMILY: LazyLock<DocFamilyToken<State, i64, RewindableConversation>> = LazyLock::new(|| {
    let scope = RewindableConversation(DocumentFork::Initial);
    define_doc_family(DocFamilySpec::new("t.conversation-family", 1, scope, seeded, |_, _, _| Ok(false))).unwrap()
});
static TASK_FAMILY: LazyLock<DocFamilyToken<State, i64, TaskScope>> = LazyLock::new(|| {
    define_doc_family(DocFamilySpec::new("t.task-family", 1, TaskScope, seeded, |_, _, _| Ok(false))).unwrap()
});

struct Work;

impl TaskTemplate for Work {
    fn name(&self) -> &str {
        "t.work"
    }
    fn version(&self) -> u32 {
        1
    }
    fn initial(&self, _input: &Json) -> DurableResult<Json> {
        Ok(json(r#"{"phase":"start"}"#))
    }
}

#[test]
fn validates_persisted_version_semantics() {
    let zero = define_doc(DocSpec::new("k", 0, SessionScope, initial, |_, _, _| Ok(false)));
    assert!(message(zero).contains("positive integer"));
    // Upstream also rejects version 1.5; a `u32` version cannot hold a fraction.
    assert_eq!(
        define_doc(DocSpec::new("", 1, SessionScope, initial, |_, _, _| Ok(false))).unwrap().definition().kind,
        ""
    );
    assert_eq!(SESSION_DOC.definition().kind, "t.session");
}

/// Every owner, key, and seed form. Rejected upstream and rejected by the Rust compiler: a Session document with
/// an owner, a conversation document without one, family access without a seed or with a seed of another type,
/// and family retirement with a seed.
async fn every_overload(tx: Tx, conversation_id: ConversationId, task_id: TaskId) -> DurableResult<()> {
    let _: Draft<State> = tx.doc(&SESSION_DOC, ()).await?;
    let _: Draft<State> = tx.doc(&LATEST_DOC, conversation_id).await?;
    let _: Draft<State> = tx.doc(&REWINDABLE_DOC, conversation_id).await?;
    let _: Draft<State> = tx.doc(&TASK_DOC, task_id).await?;
    let _: Draft<State> = tx.family_doc(&SESSION_FAMILY, (), "k", &1).await?;
    let _: Draft<State> = tx.family_doc(&CONVERSATION_FAMILY, conversation_id, "k", &1).await?;
    let _: Draft<State> = tx.family_doc(&TASK_FAMILY, task_id, "k", &1).await?;
    tx.retire_doc(&SESSION_DOC, ()).await?;
    tx.retire_doc(&LATEST_DOC, conversation_id).await?;
    tx.retire_doc(&TASK_DOC, task_id).await?;
    tx.retire_family_doc(&SESSION_FAMILY, (), "k").await?;
    tx.retire_family_doc(&CONVERSATION_FAMILY, conversation_id, "k").await?;
    tx.retire_family_doc(&TASK_FAMILY, task_id, "k").await
}

#[tokio::test]
async fn types_every_owner_key_and_seed_overload() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let options = TaskOptions::conversation(conversation_id);
    let task_id = test
        .session
        .commit(move |tx| async move { tx.create_task(Arc::new(Work), Json::Null, options).await }, &context())
        .await
        .unwrap();
    test.session.commit(move |tx| every_overload(tx, conversation_id, task_id), &context()).await.unwrap();
    let ctx = context();
    let session = &test.session;
    let _: Option<Json> = session.snapshot(&SESSION_DOC, (), &ctx).await.unwrap();
    let _: Option<Json> = session.snapshot(&LATEST_DOC, ROOT_CONVERSATION_ID, &ctx).await.unwrap();
    let _: Option<Json> = session.snapshot(&TASK_DOC, task_id, &ctx).await.unwrap();
    let _: Option<Json> = session.family_snapshot(&SESSION_FAMILY, (), "k", &ctx).await.unwrap();
    let family = session.family_snapshot(&CONVERSATION_FAMILY, ROOT_CONVERSATION_ID, "k", &ctx).await;
    let _: Option<Json> = family.unwrap();
    let _: Option<Json> = session.family_snapshot(&TASK_FAMILY, task_id, "k", &ctx).await.unwrap();
    // Snapshots never take a creation seed: `family_snapshot` has no seed parameter.
    test.session.close(&ctx).await.unwrap();
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Note {
    text: String,
}

#[tokio::test]
async fn types_historical_reads_states_watches_and_typed_entries() {
    let note = define_entry::<Note>("t.note").unwrap();
    let marker = define_entry::<()>("t.marker").unwrap();
    assert!(message(define_entry::<()>("")).contains("non-empty"));
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let (note_token, marker_token) = (note.clone(), marker.clone());
    let (raw, typed, marked) = test
        .session
        .commit(
            move |tx| async move {
                tx.doc(&REWINDABLE_DOC, conversation_id).await?;
                tx.doc(&SESSION_DOC, ()).await?;
                let raw = tx.append_entry(conversation_id, EntryDraft::new("t.raw")).await?;
                let draft = TypedEntryDraft::new(Note { text: "x".into() });
                let typed = tx.append_typed_entry(&note_token, conversation_id, draft).await?;
                let marked = tx.append_typed_entry(&marker_token, conversation_id, TypedEntryDraft::new(())).await?;
                Ok((raw, typed, marked))
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(typed.data.text, "x");
    assert_eq!(typed.record.kind, "t.note");
    assert_eq!(typed.record.data, Some(json(r#"{"text":"x"}"#)));
    assert!(marked.record.data.is_none());
    assert!(note.is(Some(&typed.record)));
    assert!(!note.is(Some(&raw)));
    assert!(!note.is(None));
    let (at, raw_id, typed_id) = (typed.record.id, raw.id, typed.record.id);
    let note_token = note.clone();
    let (read, other, plain) = test
        .session
        .commit(
            move |tx| async move {
                let read = tx.typed_entry(&note_token, typed_id).await?;
                let other = tx.typed_entry(&note_token, raw_id).await?;
                let plain = tx.entry(raw_id).await?;
                Ok((read, other, plain))
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(read.unwrap().data, Note { text: "x".into() });
    assert!(other.is_none());
    assert_eq!(plain.unwrap(), raw);
    // Latest, Session, and task documents keep no history: `snapshot_as_of` requires a rewindable scope.
    let ctx = context();
    let session = &test.session;
    let _: Option<Json> = session.snapshot_as_of(&REWINDABLE_DOC, conversation_id, at, &ctx).await.unwrap();
    let family = session.family_snapshot_as_of(&CONVERSATION_FAMILY, conversation_id, "k", at, &ctx).await;
    assert!(family.unwrap().is_none());
    let state = session.document_state(&SESSION_DOC, (), &ctx).await.unwrap().unwrap();
    state.dispose().unwrap();
    assert!(session.family_document_state(&SESSION_FAMILY, (), "k", &ctx).await.unwrap().is_none());
    let watch = session.watch_doc(&SESSION_DOC, (), &ctx).await.unwrap().unwrap();
    watch.stop().await;
    assert!(session.family_watch_doc(&TASK_FAMILY, TaskId(1), "k", &ctx).await.unwrap().is_none());
}
