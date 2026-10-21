//! Pending acquisitions, revocation, validation, rollback, Storage failure, publication, and close.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use pi_chord::{BoxFuture, Context, Json};
use pi_durable::session::{Draft, TransactionScope, Tx};
use pi_durable::*;
use serde::{Deserialize, Serialize};

use super::support::*;
use super::*;

type Pending<T = Live> = Arc<Mutex<Option<BoxFuture<DurableResult<Draft<T>>>>>>;

fn pending_task_record(conversation_id: ConversationId) -> TaskRecord {
    TaskRecord {
        id: TaskId(999),
        conversation_id,
        kind: "missing".into(),
        version: 1,
        input: Json::Null,
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: json(r#"{"phase":"start"}"#) },
        memos: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        trace: Default::default(),
        links: Default::default(),
    }
}

#[tokio::test]
async fn rejects_a_callback_that_succeeds_with_a_pending_acquisition_and_drains_it() {
    let (test, conversation_id) = setup_live().await;
    test.session.unload_documents().await.unwrap();
    let gate = test.storage.hold_find_document();
    let commits = test.storage.commit_count();
    let pending: Pending = Arc::default();
    let slot = pending.clone();
    let commit = tokio::spawn(test.session.commit(
        move |tx| {
            *slot.lock().unwrap() = Some(Box::pin(tx.doc(&LIVE_DOC, conversation_id)));
            async { Ok(()) }
        },
        &context(),
    ));
    gate.entered().await;
    flush().await;
    // The line stays held until the late acquisition settles.
    assert!(!commit.is_finished());
    gate.release();
    assert!(message(commit.await.unwrap()).contains("pending Tx operations"));
    let late = pending.lock().unwrap().take().unwrap();
    assert!(message(late.await).contains("Transaction has settled"));
    assert_eq!(test.storage.commit_count(), commits);
    set_message(&test.session, conversation_id, "after").await;
    assert_eq!(live_snapshot(&test.session, conversation_id).await.unwrap()["message"], Json::from("after"));
}

#[tokio::test]
async fn does_not_initialize_or_mint_for_an_absent_acquisition_that_finishes_after_settlement() {
    let test = open_test_session();
    let initialized = Arc::new(AtomicUsize::new(0));
    let counter = initialized.clone();
    let late_doc = define_doc(DocSpec::new(
        "test.late",
        1,
        SessionScope,
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Counter { count: 0.0 }
        },
        |_, _, _| Ok(false),
    ))
    .unwrap();
    let gate = test.storage.hold_find_document();
    let mints = test.storage.mints();
    let pending: Pending<Counter> = Arc::default();
    let slot = pending.clone();
    let commit = test.session.commit(
        move |tx| {
            *slot.lock().unwrap() = Some(Box::pin(tx.doc(&late_doc, ())));
            async { Ok(()) }
        },
        &context(),
    );
    gate.entered().await;
    gate.release();
    assert!(message(commit.await).contains("pending Tx operations"));
    let late = pending.lock().unwrap().take().unwrap();
    assert!(message(late.await).contains("Transaction has settled"));
    assert_eq!(initialized.load(Ordering::SeqCst), 0);
    assert_eq!(test.storage.mints(), mints);
}

#[tokio::test]
async fn rejects_with_the_callback_error_when_it_fails_with_a_pending_acquisition() {
    let (test, conversation_id) = setup_live().await;
    test.session.unload_documents().await.unwrap();
    let gate = test.storage.hold_find_document();
    let pending: Pending = Arc::default();
    let slot = pending.clone();
    let commit = test.session.commit(
        move |tx| {
            *slot.lock().unwrap() = Some(Box::pin(tx.doc(&LIVE_DOC, conversation_id)));
            async { Err::<(), _>(DurableError::error("callback failed")) }
        },
        &context(),
    );
    gate.entered().await;
    gate.release();
    assert_eq!(message(commit.await), "callback failed");
    let late = pending.lock().unwrap().take().unwrap();
    assert!(message(late.await).contains("Transaction has settled"));
}

#[tokio::test]
async fn rejects_tx_use_after_the_callback_settles() {
    let (test, conversation_id) = setup_live().await;
    let captured: Tx =
        test.session.commit_with(|tx| async move { Ok(tx) }, &context(), TransactionScope::default()).await.unwrap();
    assert!(message(captured.doc(&LIVE_DOC, conversation_id).await).contains("Transaction has settled"));
    assert!(message(captured.conversation(conversation_id).await).contains("Transaction has settled"));
    assert!(message(captured.set_task(pending_task_record(conversation_id))).contains("Transaction has settled"));
}

#[tokio::test]
async fn rejects_tokens_whose_semantics_or_version_disagree_with_the_stored_incarnation() {
    let (test, conversation_id) = setup_live().await;
    let snapshot = test.session.snapshot(&REWINDABLE_LIVE_DOC, conversation_id, &context()).await;
    assert!(message(snapshot).contains("does not match the supplied definition semantics"));
    let rewound = test
        .session
        .commit(move |tx| async move { tx.doc(&REWINDABLE_LIVE_DOC, conversation_id).await.map(drop) }, &context())
        .await;
    assert!(message(rewound).contains("does not match the supplied definition semantics"));
    let newer = test
        .session
        .commit(move |tx| async move { tx.doc(&LIVE_DOC_V2, conversation_id).await.map(drop) }, &context())
        .await;
    assert!(message(newer).contains("requires migration from version 1"));
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct NotJson {
    at: f64,
}

#[tokio::test]
async fn rejects_non_json_initializer_values_and_draft_placements_before_storage_admission() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    // A non-finite number stands in for upstream's `Date`, which strict JSON rejects the same way.
    let nan_doc =
        define_doc(DocSpec::new("test.date", 1, SessionScope, || NotJson { at: f64::NAN }, |_, _, _| Ok(false)))
            .unwrap();
    let created = test.session.commit(move |tx| async move { tx.doc(&nan_doc, ()).await.map(drop) }, &context()).await;
    assert!(message(created).contains("strict JSON"));
    let commits = test.storage.commit_count();
    let placed = test
        .session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.nested.count = f64::INFINITY)
            },
            &context(),
        )
        .await;
    assert!(message(placed).contains("strict JSON"));
    assert_eq!(test.storage.commit_count(), commits);
    assert!(live_snapshot(&test.session, conversation_id).await.is_none());
}

#[tokio::test]
async fn rolls_back_prepared_documents_when_batch_assembly_fails() {
    let (test, conversation_id) = setup_live().await;
    test.session
        .commit(|tx| async move { tx.doc(&COUNTER_DOC, ()).await?.edit(|c| c.count = 1.0) }, &context())
        .await
        .unwrap();
    let live = live_snapshot(&test.session, conversation_id).await.unwrap();
    let counter = test.session.snapshot(&COUNTER_DOC, (), &context()).await.unwrap().unwrap();
    let commits = test.storage.commit_count();
    let result = test
        .session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some("lost".into()))?;
                tx.doc(&COUNTER_DOC, ()).await?.edit(|c| c.count = 2.0)?;
                // Replacing a missing task fails during assembly, after every change was prepared.
                tx.set_task(pending_task_record(conversation_id))
            },
            &context(),
        )
        .await;
    assert_eq!(message(result), "Task 999 does not exist");
    assert_eq!(test.storage.commit_count(), commits);
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(&live));
    assert!(test.session.snapshot(&COUNTER_DOC, (), &context()).await.unwrap().unwrap().same_ref(&counter));
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some("next".into()))?;
                tx.doc(&COUNTER_DOC, ()).await?.edit(|c| c.count = 3.0)
            },
            &context(),
        )
        .await
        .unwrap();
    let counter = test.session.snapshot(&COUNTER_DOC, (), &context()).await.unwrap().unwrap();
    assert_eq!(counter["count"], Json::from(3));
}

#[tokio::test]
async fn fails_the_session_after_a_storage_failure_and_publishes_nothing() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let before = live_snapshot(&test.session, conversation_id).await.unwrap();
    let published = test.publication_count();
    test.storage.fail_next_commit(DurableError::storage("disk vanished"));
    let session = test.session.clone();
    let failed = session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some("uncertain".into()))
            },
            &context(),
        )
        .await;
    assert_eq!(message(failed), "disk vanished");
    flush().await;
    assert_eq!(test.publication_count(), published);
    assert!(before.get("message").is_none());
    let snapshot = test.session.snapshot(&LIVE_DOC, conversation_id, &context()).await;
    assert_eq!(snapshot.unwrap_err().name(), "SessionFailed");
    let commit = test.session.commit(|_| async { Ok(()) }, &context()).await;
    assert_eq!(commit.unwrap_err().name(), "SessionFailed");
    let _ = test.session.close(&context()).await;
}

#[tokio::test]
async fn keeps_the_previous_revision_unchanged_through_storage_settlement() {
    let (test, conversation_id) = setup_live().await;
    let before = live_snapshot(&test.session, conversation_id).await.unwrap();
    let copy = Json::parse(&before.to_string()).unwrap();
    let gate = test.storage.hold_commits();
    let commit = test.session.commit(
        move |tx| async move {
            tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
                live.items.push("c".into());
                live.nested.count = 9.0;
            })
        },
        &context(),
    );
    gate.entered().await;
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(&before));
    assert_eq!(before, copy);
    gate.release();
    commit.await.unwrap();
    let after = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert_eq!(after["items"], json(r#"["a","b","c"]"#));
    assert_eq!(before, copy);
}

#[tokio::test]
async fn reloads_an_unloaded_document_from_storage() {
    let (test, conversation_id) = setup_live().await;
    let loaded = live_snapshot(&test.session, conversation_id).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let reloaded = live_snapshot(&test.session, conversation_id).await.unwrap();
    // Immutable Rust JSON lets MemoryStorage share the stored root, so only equality is required.
    assert_eq!(reloaded, loaded);
    test.session
        .commit(
            move |tx| async move { tx.doc(&LIVE_DOC, conversation_id).await?.edit(|l| l.items.push("c".into())) },
            &context(),
        )
        .await
        .unwrap();
    test.session.unload_documents().await.unwrap();
    let items = live_snapshot(&test.session, conversation_id).await.unwrap()["items"].clone();
    assert_eq!(items, json(r#"["a","b","c"]"#));
}

#[tokio::test]
async fn delivers_complete_publications_synchronously_after_adoption() {
    let (test, conversation_id) = setup_live().await;
    let published = test.publication_count();
    let delivered: Arc<Mutex<Option<Context>>> = Arc::default();
    let sink = delivered.clone();
    let unsubscribe = test
        .session
        .subscribe_commits(move |_, context| {
            *sink.lock().unwrap() = Some(context.clone());
            Ok(())
        })
        .unwrap();
    let commit_context = context();
    let result = test
        .session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some("m".into()))?;
                Ok("done")
            },
            &commit_context,
        )
        .await
        .unwrap();
    assert_eq!(result, "done");
    assert!(delivered.lock().unwrap().as_ref().unwrap().ptr_eq(&commit_context));
    assert_eq!(test.publication_count(), published + 1);
    unsubscribe.unsubscribe();
}

#[tokio::test]
async fn publishes_close_synchronously_and_supports_unsubscription() {
    let test = open_test_session();
    let calls: Arc<Mutex<Vec<&str>>> = Arc::default();
    let active = calls.clone();
    test.session.subscribe_close(move || active.lock().unwrap().push("active")).unwrap();
    let removed = calls.clone();
    let unsubscribe = test.session.subscribe_close(move || removed.lock().unwrap().push("removed")).unwrap();
    unsubscribe.unsubscribe();
    test.session.close(&context()).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["active"]);
}

#[tokio::test]
async fn settles_admitted_commits_before_close_and_rejects_later_admission() {
    let (test, conversation_id) = setup_live().await;
    let gate = test.storage.hold_commits();
    let ctx = context();
    let edit = |text: &'static str| {
        move |tx: Tx| async move { tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.message = Some(text.into())) }
    };
    let commit = test.session.commit(edit("admitted"), &ctx);
    gate.entered().await;
    let queued = test.session.commit(edit("queued"), &ctx);
    let admitted_snapshot = test.session.family_snapshot(&MEMBER_DOC, (), "absent", &ctx);
    let closed = test.session.close(&ctx);
    assert!(message(test.session.commit(|_| async { Ok(()) }, &ctx).await).contains("closed"));
    assert!(message(test.session.snapshot(&LIVE_DOC, conversation_id, &ctx).await).contains("closed"));
    gate.release();
    commit.await.unwrap();
    queued.await.unwrap();
    assert!(admitted_snapshot.await.unwrap().is_none());
    closed.await.unwrap();
    let address =
        DocumentAddress { kind: "test.live".into(), scope: DocumentScope::Conversation { conversation_id }, key: None };
    assert!(test.storage.find_document(&address, DocumentPoint::Current, &ctx).await.is_err());
}
