//! Session document migrations.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;

use super::support::*;
use super::*;
use crate::support::{ValueCall, on_value};

type Frames = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

fn counted(calls: &Arc<AtomicUsize>) -> Arc<AtomicUsize> {
    calls.clone()
}

#[tokio::test]
async fn migrates_read_only_once_per_cold_load_copies_the_callback_result_and_writes_nothing() {
    let old = define_doc(session_doc("migration.read-only", 1, r#"{"count":2}"#)).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = counted(&calls);
    let current =
        define_doc(session_doc("migration.read-only", 3, r#"{"count":0,"labels":[]}"#).migrate(move |value, from| {
            assert_eq!(from, 1);
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(json(&format!(r#"{{"count":{},"labels":["migrated"]}}"#, value["count"])))
        }))
        .unwrap();
    let test = open_test_session();
    touch(&test.session, &old, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    let first = snapshot(&test.session, &current).await.unwrap().unwrap();
    assert!(snapshot(&test.session, &current).await.unwrap().unwrap().same_ref(&first));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(test.storage.commit_count(), commits);
    // Upstream mutates the callback's returned object here; a Rust callback hands over an owned value.
    assert_eq!(first, json(r#"{"count":2,"labels":["migrated"]}"#));
    test.session.unload_documents().await.unwrap();
    let second = snapshot(&test.session, &current).await.unwrap().unwrap();
    assert!(!second.same_ref(&first));
    assert_eq!(second, first);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(test.storage.commit_count(), commits);
}

#[tokio::test]
async fn writes_the_required_base_on_the_first_successful_transaction_then_writes_deltas() {
    let old = define_doc(session_doc("migration.transition", 1, r#"{"count":4}"#)).unwrap();
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let counter = counted(&checkpoints);
    let current = define_doc(
        session_doc_with("migration.transition", 3, r#"{"count":0}"#, move |_, _, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        })
        .migrate(|value, from| {
            Ok(json(&format!(r#"{{"count":{}}}"#, value["count"].as_i64().unwrap() + i64::from(from) - 1)))
        }),
    )
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &old, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    // An observer of the older shape.
    let watch = test.session.watch_doc(&old, (), &context()).await.unwrap().unwrap();
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(on_value(move |ValueCall { value, ops, .. }, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        }))
        .unwrap();
    assert_eq!(snapshot(&test.session, &current).await.unwrap().unwrap(), json(r#"{"count":4}"#));
    let migrated = snapshot(&test.session, &current).await.unwrap().unwrap();
    // An observer of the new shape: the migration changes nothing it sees.
    let watch_current = test.session.watch_doc(&current, (), &context()).await.unwrap().unwrap();
    let current_frames: Arc<Mutex<Vec<Json>>> = Arc::default();
    let sink = current_frames.clone();
    watch_current
        .start(on_value(move |ValueCall { value, .. }, _| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        }))
        .unwrap();
    touch(&test.session, &current, ()).await.unwrap();
    flush().await;
    let Some(DocumentContent::Base { version: 3, value }) = first_content(&test) else { panic!("expected a base") };
    assert_eq!(value, json(r#"{"count":4}"#));
    assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
    // The migration-only base is published, so the older-shape watch receives the new value as a root replacement.
    let published = document_changes(&test.last_publication());
    let DocumentCommitChange::Change { version: 3, value, ops, .. } = &published[0] else { panic!() };
    assert_eq!(*value, json(r#"{"count":4}"#));
    assert!(ops.is_empty());
    let expected = (json(r#"{"count":4}"#), vec![Op::Replace(json(r#"{"count":4}"#))]);
    assert_eq!(*frames.lock().unwrap(), vec![expected]);
    assert!(current_frames.lock().unwrap().is_empty());
    watch.stop().await;
    watch_current.stop().await;
    assert!(snapshot(&test.session, &current).await.unwrap().unwrap().same_ref(&migrated));

    set_field(&test.session, &current, (), "count", Json::from(7)).await.unwrap();
    assert!(matches!(first_content(&test), Some(DocumentContent::Delta { version: 3, .. })));
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    test.session.unload_documents().await.unwrap();
    assert_eq!(snapshot(&test.session, &current).await.unwrap().unwrap(), json(r#"{"count":7}"#));
}

#[tokio::test]
async fn rolls_migration_and_edits_back_with_the_callback_then_coalesces_later_edits_into_one_base() {
    let old = define_doc(session_doc("migration.rollback", 1, r#"{"count":1}"#)).unwrap();
    let migrations = Arc::new(AtomicUsize::new(0));
    let counter = counted(&migrations);
    let current =
        define_doc(session_doc("migration.rollback", 2, r#"{"count":0,"migrated":false}"#).migrate(move |value, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(json(&format!(r#"{{"count":{},"migrated":true}}"#, value["count"])))
        }))
        .unwrap();
    let test = open_test_session();
    touch(&test.session, &old, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    let token = current.clone();
    let failed = test
        .session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|d| d.set("count", 8))??;
                Err::<(), _>(DurableError::error("rollback"))
            },
            &context(),
        )
        .await;
    assert_eq!(message(failed), "rollback");
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(snapshot(&test.session, &current).await.unwrap().unwrap(), json(r#"{"count":1,"migrated":true}"#));
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move {
                let draft = tx.doc(&token, ()).await?;
                draft.update(|d| d.set("count", 9))??;
                draft.update(|d| d.set("migrated", false))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = document_writes(&test.storage.last_commit());
    assert_eq!(writes.len(), 1);
    let StorageWrite::DocumentChange { content: DocumentContent::Base { version: 2, value: base }, .. } = &writes[0]
    else {
        panic!("expected a version base")
    };
    assert_eq!(*base, json(r#"{"count":9,"migrated":false}"#));
    flush().await;
    let DocumentCommitChange::Change { value, ops, .. } = &document_changes(&test.last_publication())[0] else {
        panic!()
    };
    assert!(value.same_ref(&snapshot(&test.session, &current).await.unwrap().unwrap()));
    assert!(value.same_ref(base));
    assert!(!ops.is_empty());
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn strict_checks_migration_results_before_tracker_ownership_and_remains_usable_after_rejection() {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Invalid {
        invalid: f64,
    }
    let old = define_doc(session_doc("migration.invalid", 1, r#"{"count":1}"#)).unwrap();
    // A non-finite number stands in for upstream's `Date`, which strict JSON rejects the same way.
    let invalid = define_doc(
        DocSpec::new("migration.invalid", 2, SessionScope, || Invalid { invalid: 0.0 }, |_, _, _| Ok(false))
            .migrate(|_, _| Ok(Invalid { invalid: f64::NAN })),
    )
    .unwrap();
    let other = define_doc(session_doc("migration.invalid.other", 1, r#"{"ok":true}"#)).unwrap();
    let test = open_test_session();
    touch(&test.session, &old, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    assert!(message(test.session.snapshot(&invalid, (), &context()).await).contains("strict JSON"));
    let token = invalid.clone();
    let failed = test.session.commit(move |tx| async move { tx.doc(&token, ()).await.map(drop) }, &context()).await;
    assert!(message(failed).contains("strict JSON"));
    assert_eq!(test.storage.commit_count(), commits);
    touch(&test.session, &other, ()).await.unwrap();
    assert_eq!(snapshot(&test.session, &other).await.unwrap().unwrap(), json(r#"{"ok":true}"#));
}

#[tokio::test]
async fn rejects_newer_stored_versions_and_older_versions_without_migration_for_snapshots_and_transactions() {
    let v2 = define_doc(session_doc("migration.compatibility", 2, r#"{"count":2}"#)).unwrap();
    let v1 = define_doc(session_doc("migration.compatibility", 1, r#"{"count":1}"#)).unwrap();
    let v3 = define_doc(session_doc("migration.compatibility", 3, r#"{"count":3}"#)).unwrap();
    let test = open_test_session();
    touch(&test.session, &v2, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    assert!(message(snapshot(&test.session, &v1).await).contains("newer version 2 than 1"));
    assert!(message(touch(&test.session, &v1, ()).await).contains("newer version 2 than 1"));
    assert!(message(snapshot(&test.session, &v3).await).contains("requires migration from version 2"));
    assert!(message(touch(&test.session, &v3, ()).await).contains("requires migration from version 2"));
    assert_eq!(test.storage.commit_count(), commits);
}

#[tokio::test]
async fn persists_a_required_migration_base_before_retirement_without_consulting_the_checkpoint_predicate() {
    let old = define_doc(session_doc("migration.retire", 1, r#"{"count":1}"#)).unwrap();
    let current = define_doc(
        session_doc_with("migration.retire", 2, r#"{"count":0}"#, |_, _, _| Err(DurableError::error("must not run")))
            .migrate(|value, _| Ok(json(&format!(r#"{{"count":{}}}"#, value["count"])))),
    )
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &old, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?;
                tx.retire_doc(&token, ()).await
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = document_writes(&test.storage.last_commit());
    assert_eq!(writes.len(), 2);
    let StorageWrite::DocumentChange { content: DocumentContent::Base { version: 2, value }, .. } = &writes[0] else {
        panic!()
    };
    assert_eq!(*value, json(r#"{"count":1}"#));
    assert!(matches!(writes[1], StorageWrite::DocumentRetire { .. }));
}

#[tokio::test]
async fn leaves_unaccessed_older_documents_and_unavailable_definitions_untouched() {
    let first_v1 = define_doc(session_doc("migration.lazy.first", 1, r#"{"count":1}"#)).unwrap();
    let second_v1 = define_doc(session_doc("migration.lazy.second", 1, r#"{"count":2}"#)).unwrap();
    let second_migrations = Arc::new(AtomicUsize::new(0));
    let first_v2 = define_doc(
        session_doc("migration.lazy.first", 2, r#"{"count":0}"#)
            .migrate(|value, _| Ok(json(&format!(r#"{{"count":{}}}"#, value["count"])))),
    )
    .unwrap();
    let counter = counted(&second_migrations);
    let _second_v2 = define_doc(session_doc("migration.lazy.second", 2, r#"{"count":0}"#).migrate(move |value, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(json(&format!(r#"{{"count":{}}}"#, value["count"])))
    }))
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &first_v1, ()).await.unwrap();
    touch(&test.session, &second_v1, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    assert_eq!(snapshot(&test.session, &first_v2).await.unwrap().unwrap(), json(r#"{"count":1}"#));
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(second_migrations.load(Ordering::SeqCst), 0);
    let ctx = context();
    let address = DocumentAddress { kind: "migration.lazy.second".into(), scope: DocumentScope::Session, key: None };
    let record = test.storage.find_document(&address, DocumentPoint::Current, &ctx).await.unwrap().unwrap();
    let stored = test.storage.document(record.id, DocumentPoint::Current, &ctx).await.unwrap().unwrap();
    assert_eq!(stored.version, 1);
}
