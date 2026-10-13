//! Port of upstream `test/session-states.test.ts`.

mod support;

use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::Json;
use pi_chord::delta::Op;
use pi_chord::state::StateListener;
use pi_durable::documents::*;
use pi_durable::session::Session;
use pi_durable::*;
use support::*;

static STATE_DOC: LazyLock<DocToken<Json, SessionScope>> = LazyLock::new(|| {
    let initial = || json(r#"{"value":0,"retained":{"label":"stable"}}"#);
    define_doc(DocSpec::new("state.state", 1, SessionScope, initial)).unwrap()
});

static FAMILY_DOC: LazyLock<DocFamilyToken<Json, String, SessionScope>> = LazyLock::new(|| {
    let initial = |seed: String| json(&format!(r#"{{"value":{}}}"#, seed.len()));
    define_doc_family(DocFamilySpec::new("state.family", 1, SessionScope, initial)).unwrap()
});

async fn create_state() -> TestSession {
    let test = open_test_session();
    test.session.commit(|tx| async move { tx.doc(&STATE_DOC, ()).await.map(drop) }, &context()).await.unwrap();
    flush().await;
    test
}

async fn set_value(session: &Session, value: i64) {
    session
        .commit(
            move |tx| async move { tx.doc(&STATE_DOC, ()).await?.update(|s| s.set("value", value))?.map_err(Into::into) },
            &context(),
        )
        .await
        .unwrap();
}

async fn state_of(session: &Session) -> session::DocumentState {
    session.document_state(&STATE_DOC, (), &context()).await.unwrap().unwrap()
}

/// Subscribe and record `(value field, delivery sequence)` pairs.
fn deliveries(state: &session::DocumentState) -> Arc<Mutex<Vec<(Json, u64)>>> {
    let deliveries: Arc<Mutex<Vec<(Json, u64)>>> = Arc::default();
    let sink = deliveries.clone();
    state.subscribe(StateListener::sync(move |value, _, delivery| {
        sink.lock().unwrap().push((value["value"].clone(), delivery.sequence));
        Ok(())
    }));
    deliveries
}

/// Record every exact operation batch the state publishes.
fn operations(state: &session::DocumentState) -> Arc<Mutex<Vec<Vec<Op>>>> {
    let batches: Arc<Mutex<Vec<Vec<Op>>>> = Arc::default();
    let sink = batches.clone();
    state.member().subscribe_ops(move |ops, _, _| {
        sink.lock().unwrap().push(ops.to_vec());
        Ok(())
    });
    batches
}

#[tokio::test]
async fn never_creates_an_absent_document() {
    let test = open_test_session();
    let commits = test.storage.commit_count();
    assert!(test.session.document_state(&STATE_DOC, (), &context()).await.unwrap().is_none());
    assert!(test.session.family_document_state(&FAMILY_DOC, (), "missing", &context()).await.unwrap().is_none());
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.storage.mints(), 0);
}

#[tokio::test]
async fn returns_an_immediately_hydrated_read_only_state_with_contiguous_chord_deliveries() {
    let test = create_state().await;
    let baseline = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    let state = state_of(&test.session).await;
    let seen = deliveries(&state);
    assert!(state.value().same_ref(&baseline));
    set_value(&test.session, 1).await;
    set_value(&test.session, 2).await;
    flush().await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.iter().map(|d| d.1).collect::<Vec<_>>(), [0, 1, 2]);
    assert_eq!(seen.iter().map(|d| d.0.clone()).collect::<Vec<_>>(), [Json::from(0), Json::from(1), Json::from(2)]);
    let snapshot = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    assert!(state.value().same_ref(&snapshot));
    state.dispose().unwrap();
}

#[tokio::test]
async fn creates_independent_disposable_states_for_one_incarnation() {
    let test = create_state().await;
    let first = state_of(&test.session).await;
    let second = state_of(&test.session).await;
    set_value(&test.session, 1).await;
    flush().await;
    assert_eq!(first.value()["value"], Json::from(1));
    assert_eq!(second.value()["value"], Json::from(1));
    first.dispose().unwrap();
    set_value(&test.session, 2).await;
    flush().await;
    assert_eq!(first.value()["value"], Json::from(1));
    assert_eq!(second.value()["value"], Json::from(2));
    second.dispose().unwrap();
}

#[tokio::test]
async fn shares_exact_committed_value_and_operation_references_with_chord() {
    let test = create_state().await;
    let state = state_of(&test.session).await;
    let received = operations(&state);
    set_value(&test.session, 4).await;
    flush().await;
    let DocumentCommitChange::Document { value, ops, .. } = &document_changes(&test.last_publication())[0] else {
        panic!()
    };
    assert!(state.value().same_ref(value.as_ref().unwrap()));
    assert_eq!(received.lock().unwrap().last().unwrap(), ops);
    let snapshot = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    assert!(state.value()["retained"].same_ref(&snapshot["retained"]));
    state.dispose().unwrap();
}

#[tokio::test]
async fn captures_a_late_baseline_without_redelivering_an_already_covered_commit() {
    let test = create_state().await;
    set_value(&test.session, 1).await;
    let state = state_of(&test.session).await;
    let seen = deliveries(&state);
    flush().await;
    assert_eq!(state.value()["value"], Json::from(1));
    assert_eq!(seen.lock().unwrap().iter().map(|d| d.1).collect::<Vec<_>>(), [0]);
    state.dispose().unwrap();
}

#[tokio::test]
async fn publishes_null_retirement_and_never_follows_a_replacement_incarnation() {
    let test = create_state().await;
    let old = state_of(&test.session).await;
    let retirement = operations(&old);
    test.session
        .commit(
            |tx| async move {
                tx.retire_doc(&STATE_DOC, ()).await?;
                tx.doc(&STATE_DOC, ()).await?.update(|s| s.set("value", 10))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert!(old.value().is_null());
    assert_eq!(retirement.lock().unwrap().last().unwrap(), &vec![Op::Replace(Json::Null)]);
    let replacement = state_of(&test.session).await;
    assert_eq!(replacement.value()["value"], Json::from(10));
    set_value(&test.session, 11).await;
    flush().await;
    assert!(old.value().is_null());
    assert_eq!(replacement.value()["value"], Json::from(11));
    old.dispose().unwrap();
    replacement.dispose().unwrap();
}

#[tokio::test]
async fn cold_loads_a_definition_free_fork_copy() {
    let copied =
        define_doc(DocSpec::new("state.copied", 1, LatestConversation(LatestFork::Current), || json(r#"{"value":0}"#)))
            .unwrap();
    let test = open_test_session();
    let parent_id = create_conversation(&test.session).await;
    let token = copied.clone();
    let entry = test
        .session
        .commit(
            move |tx| async move {
                let created = tx.append_entry(parent_id, EntryDraft::new("point")).await?;
                tx.doc(&token, parent_id).await?.update(|v| v.set("value", 7))??;
                Ok(created)
            },
            &context(),
        )
        .await
        .unwrap();
    let child_id = test
        .session
        .commit(
            move |tx| async move {
                Ok(tx.fork_conversation(parent_id, entry.id, ConversationOwnership::Ownerless).await?.id)
            },
            &context(),
        )
        .await
        .unwrap();
    let reads = test.storage.document_reads();
    let state = test.session.document_state(&copied, child_id, &context()).await.unwrap().unwrap();
    assert_eq!(state.value(), json(r#"{"value":7}"#));
    assert!(test.storage.document_reads() > reads);
    state.dispose().unwrap();
}

#[tokio::test]
async fn hydrates_a_migrated_tracker_without_writing_and_skips_an_equal_version_base_update() {
    let old = define_doc(DocSpec::new("state.migration", 1, SessionScope, || json(r#"{"value":3}"#))).unwrap();
    let current = define_doc(
        DocSpec::new("state.migration", 2, SessionScope, || json(r#"{"value":0,"migrated":false}"#))
            .migrate(|value, _| Ok(json(&format!(r#"{{"value":{},"migrated":true}}"#, value["value"])))),
    )
    .unwrap();
    let test = open_test_session();
    test.session.commit(move |tx| async move { tx.doc(&old, ()).await.map(drop) }, &context()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    let state = test.session.document_state(&current, (), &context()).await.unwrap().unwrap();
    assert_eq!(state.value(), json(r#"{"value":3,"migrated":true}"#));
    assert_eq!(test.storage.commit_count(), commits);
    let baseline = state.value();
    let token = current.clone();
    test.session.commit(move |tx| async move { tx.doc(&token, ()).await.map(drop) }, &context()).await.unwrap();
    flush().await;
    assert_eq!(test.storage.commit_count(), commits + 1);
    assert!(state.value().same_ref(&baseline));
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move { tx.doc(&token, ()).await?.update(|s| s.set("value", 4))?.map_err(Into::into) },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert_eq!(state.value(), json(r#"{"value":4,"migrated":true}"#));
    state.dispose().unwrap();
}

#[tokio::test]
async fn continues_from_exact_committed_values_after_the_tracker_cache_unloads() {
    let test = create_state().await;
    let state = state_of(&test.session).await;
    let baseline = state.value();
    let reads = test.storage.document_reads();
    test.session.unload_documents().await.unwrap();
    let reloaded = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    assert_eq!(reloaded, baseline);
    assert!(test.storage.document_reads() > reads);
    set_value(&test.session, 6).await;
    flush().await;
    assert_eq!(state.value()["value"], Json::from(6));
    state.dispose().unwrap();
}

// Upstream also checks that values are not frozen with `Object.isFrozen`. Rust JSON is immutable by type, so only
// the shared identity is checked here.
#[tokio::test]
async fn exposes_trusted_shared_immutable_values() {
    let test = create_state().await;
    let snapshot = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    let state = state_of(&test.session).await;
    assert!(state.value().same_ref(&snapshot));
    state.dispose().unwrap();
}
