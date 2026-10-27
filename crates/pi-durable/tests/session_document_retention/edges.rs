//! Eviction edges: history and forks, failed commits, shortened retention, observers, and migrations.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{self, SessionHooks, SessionOptions, create_session};
use pi_durable::*;
use serde_json::json;

use super::support::task_support::eventually;
use super::support::{ControlledStorage, context, create_conversation};
use super::{COUNTER, Clock, Counter, REWINDABLE, advance, bump, count_of, counter_json, emptied, open, settle_line};

/// A session document whose checkpoint predicate fails, so the commit that changes it fails while it settles.
static FAILING: LazyLock<DocToken<Counter, SessionScope>> = LazyLock::new(|| {
    let spec = DocSpec::new(
        "test.retention.failing",
        1,
        SessionScope,
        || Counter { count: 0.0 },
        |_, _, _| Err(DurableError::error("checkpoint failed")),
    );
    define_doc(spec).unwrap()
});

#[tokio::test]
async fn keeps_history_and_forks_of_rewindable_documents_after_eviction() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    let mut entries = Vec::new();
    for count in [1.0, 2.0] {
        let append = move |tx: session::Tx| async move {
            tx.doc(&REWINDABLE, conversation_id).await?.edit(|counter| counter.count = count)?;
            Ok(tx.append_entry(conversation_id, EntryDraft::new("note")).await?.id)
        };
        entries.push(test.session.commit(append, &context()).await.unwrap());
        settle_line(&test.session).await;
    }
    let ctx = context();
    let at = |entry| test.session.snapshot_as_of(&REWINDABLE, conversation_id, entry, &ctx);
    assert_eq!(at(entries[0]).await.unwrap(), Some(counter_json(1.0)));
    assert_eq!(at(entries[1]).await.unwrap(), Some(counter_json(2.0)));
    let first = entries[0];
    let fork = move |tx: session::Tx| async move {
        Ok(tx.fork_conversation(conversation_id, first, ConversationOwnership::Ownerless).await?.id)
    };
    let fork = test.session.commit(fork, &context()).await.unwrap();
    settle_line(&test.session).await;
    assert_eq!(test.session.snapshot(&REWINDABLE, fork, &ctx).await.unwrap(), Some(counter_json(1.0)));
    assert_eq!(test.session.snapshot(&REWINDABLE, conversation_id, &ctx).await.unwrap(), Some(counter_json(2.0)));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn evicts_after_a_commit_whose_settlement_fails() {
    let test = open(10_000.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    // Created first: the predicate runs only for changes to an existing document.
    test.session.commit(|tx| async move { tx.doc(&FAILING, ()).await.map(drop) }, &context()).await.unwrap();
    settle_line(&test.session).await;
    // Retention passes inside a commit whose checkpoint predicate fails while it settles.
    let clock = test.clock.clone();
    let fail = move |tx: session::Tx| async move {
        tx.doc(&FAILING, ()).await?.edit(|counter| counter.count += 1.0)?;
        advance(&clock, 1_000_000.0);
        Ok(())
    };
    let error = test.session.commit(fail, &context()).await.unwrap_err();
    assert!(error.to_string().contains("checkpoint failed"), "{error}");
    assert_eq!(test.sizes().cached, 0);
    test.session.close(&context()).await.unwrap();
}

struct Shortened(Arc<Mutex<Option<f64>>>);

impl SessionHooks for Shortened {
    fn document_retention_ms(&self) -> Option<f64> {
        *self.0.lock().unwrap()
    }
}

#[tokio::test]
async fn evicts_promptly_once_the_retention_is_shortened() {
    let clock: Clock = Arc::new(Mutex::new(1_000.0));
    let retention = Arc::new(Mutex::new(Some(600_000.0)));
    let reader = clock.clone();
    let options = SessionOptions {
        now: Some(Arc::new(move || *reader.lock().unwrap())),
        hooks: Some(Arc::new(Shortened(retention.clone()))),
        document_retention_ms: None,
        ..SessionOptions::default()
    };
    let session = create_session(Arc::new(ControlledStorage::default()), options);
    let conversation_id = create_conversation(&session).await;
    bump(&session, conversation_id, 1.0).await;
    *retention.lock().unwrap() = Some(0.0);
    advance(&clock, 2_000.0);
    // A read, no commit: the sweep pending for ten minutes from now moves up.
    count_of(&session, conversation_id).await;
    eventually(|| async {
        settle_line(&session).await;
        session.document_cache_sizes().cached == 0
    })
    .await;
    session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_keep_a_replacement_cached_for_an_observer_of_the_retired_incarnation() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    let state = test.session.document_state(&COUNTER, conversation_id, &context()).await.unwrap().unwrap();
    let retire = move |tx: session::Tx| async move { tx.retire_doc(&COUNTER, conversation_id).await };
    test.session.commit(retire, &context()).await.unwrap();
    assert!(state.value().is_null());
    bump(&test.session, conversation_id, 5.0).await;
    emptied(&test).await;
    assert_eq!(count_of(&test.session, conversation_id).await, counter_json(5.0));
    state.dispose().unwrap();
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn leaves_no_pin_behind_when_an_observer_s_load_loses_a_race_with_close() {
    let test = open(0.0);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    emptied(&test).await;
    let gate = test.storage.hold_find_document();
    let session = test.session.clone();
    let attaching = tokio::spawn(async move { session.document_state(&COUNTER, conversation_id, &context()).await });
    gate.entered().await;
    let closing = test.session.close(&context());
    gate.release();
    assert!(attaching.await.unwrap().is_err());
    closing.await.unwrap();
    let sizes = test.sizes();
    assert_eq!((sizes.cached, sizes.idle, sizes.attached), (0, 0, 0));
}

#[tokio::test]
async fn evicts_on_its_timer_without_a_commit_and_tolerates_a_detach_called_twice() {
    let options = SessionOptions { document_retention_ms: Some(20.0), ..SessionOptions::default() };
    let session = create_session(Arc::new(ControlledStorage::default()), options);
    let conversation_id = create_conversation(&session).await;
    bump(&session, conversation_id, 1.0).await;
    let state = session.document_state(&COUNTER, conversation_id, &context()).await.unwrap().unwrap();
    let _ = state.dispose();
    let _ = state.dispose();
    let until = Instant::now() + Duration::from_secs(2);
    while session.document_cache_sizes().cached > 0 && Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(session.document_cache_sizes().cached, 0);
    session.close(&context()).await.unwrap();
}

fn migrating(version: u32, counter: Option<Arc<AtomicUsize>>) -> DocToken<Json, SessionScope> {
    let initial = || Json::from(json!({ "count": 0, "migrated": true }));
    let mut spec = DocSpec::new("test.retention.migrating", version, SessionScope, initial, |_, _, _| Ok(false));
    if let Some(counter) = counter {
        spec = spec.migrate(move |value, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Json::from(json!({ "count": value["count"], "migrated": true })))
        });
    }
    define_doc(spec).unwrap()
}

#[tokio::test]
async fn migrates_a_document_again_after_its_eviction() {
    let migrations = Arc::new(AtomicUsize::new(0));
    let (v1, v2) = (migrating(1, None), migrating(2, Some(migrations.clone())));
    let test = open(0.0);
    let created = move |tx: session::Tx| async move {
        tx.doc(&v1, ()).await?.set(&Json::from(json!({ "count": 1, "migrated": true })))
    };
    test.session.commit(created, &context()).await.unwrap();
    settle_line(&test.session).await;
    let expected = Some(Json::from(json!({ "count": 1, "migrated": true })));
    assert_eq!(test.session.snapshot(&v2, (), &context()).await.unwrap(), expected);
    settle_line(&test.session).await;
    assert_eq!(test.session.snapshot(&v2, (), &context()).await.unwrap(), expected);
    // Not written back, so each cold load migrates again.
    assert_eq!(migrations.load(Ordering::SeqCst), 2);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn treats_a_retention_that_is_not_a_count_of_milliseconds_as_the_default_and_infinity_as_never() {
    for retention in [f64::NAN, -5.0] {
        let test = open(retention);
        let conversation_id = create_conversation(&test.session).await;
        bump(&test.session, conversation_id, 1.0).await;
        advance(&test.clock, 60_000.0);
        test.session.commit(|_| async { Ok(()) }, &context()).await.unwrap();
        settle_line(&test.session).await;
        // Not dropped at once, as a retention of 0 would: the ten-minute default holds it.
        assert_eq!(test.sizes().cached, 1);
        test.session.close(&context()).await.unwrap();
    }
    let test = open(f64::INFINITY);
    let conversation_id = create_conversation(&test.session).await;
    bump(&test.session, conversation_id, 1.0).await;
    advance(&test.clock, 1e15);
    test.session.commit(|_| async { Ok(()) }, &context()).await.unwrap();
    settle_line(&test.session).await;
    assert_eq!(test.sizes().cached, 1);
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_reloaded_newer_version_cached_while_an_observer_of_the_older_one_is_attached() {
    let (v1, v2) = (migrating(1, None), migrating(2, Some(Arc::new(AtomicUsize::new(0)))));
    let test = open(0.0);
    let read = v1.clone();
    test.session.commit(move |tx| async move { tx.doc(&read, ()).await.map(drop) }, &context()).await.unwrap();
    let state = test.session.document_state(&v1, (), &context()).await.unwrap().unwrap();
    let expected = Some(Json::from(json!({ "count": 0, "migrated": true })));
    assert_eq!(test.session.snapshot(&v2, (), &context()).await.unwrap(), expected);
    settle_line(&test.session).await;
    assert_eq!(test.sizes().cached, 1);
    state.dispose().unwrap();
    emptied(&test).await;
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn queues_one_sweep_job_not_one_per_document_while_a_commit_holds_the_line() {
    let test = open(0.0);
    let mut conversations = Vec::new();
    for _ in 0..50 {
        let id = create_conversation(&test.session).await;
        bump(&test.session, id, 1.0).await;
        conversations.push(id);
    }
    emptied(&test).await;
    let before = test.sizes().sweeps;
    // One commit loads 50 evicted documents from storage, each touched as it loads; the sweep job stays pending while
    // the line is held.
    let touch = move |tx: session::Tx| async move {
        for id in conversations {
            tx.doc(&COUNTER, id).await?.edit(|counter| counter.count += 1.0)?;
            tokio::time::sleep(Duration::from_millis(0)).await;
        }
        Ok(())
    };
    test.session.commit(touch, &context()).await.unwrap();
    settle_line(&test.session).await;
    assert_eq!(test.sizes().cached, 0);
    assert!(test.sizes().sweeps - before <= 2, "{} sweeps", test.sizes().sweeps - before);
    test.session.close(&context()).await.unwrap();
}
