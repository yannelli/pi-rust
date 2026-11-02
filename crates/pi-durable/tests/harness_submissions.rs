//! Port of upstream `test/harness-submissions.test.ts`.

mod support;

#[path = "harness_submissions/commits.rs"]
mod commits;

use std::sync::Arc;

use pi_ai::{Message, UserContent, UserMessage};
use pi_chord::{AbortController, ChordError, Json, with_abort_signal};
use pi_durable::entries::{ASSISTANT_ENTRY, USER_ENTRY, define_entry};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::open_sqlite_storage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
};
use pi_durable::*;
use serde_json::json;
use support::harness_support::*;
use support::{ControlledStorage, Deferred};

fn note(data: Option<serde_json::Value>) -> EntryDraft {
    EntryDraft { data: data.map(Json::from), ..EntryDraft::new("note") }
}

fn setup() -> ChatSetup {
    chat_setup(RegisterFauxProviderOptions::default())
}

#[tokio::test]
async fn appends_an_idle_write_and_settles_it_done_without_a_turn() {
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup()).await;
    let draft = SubmissionDraft::write(note(Some(json!({ "text": "x" }))));
    let submission = root.submit(draft, &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&settled).unwrap(),
        json!({ "id": submission.id, "conversationId": root.id, "type": "write", "status": "done", "entry": settled.entry })
    );
    assert!(settled.entry.is_some());
    let entries = all_entries(&root).await;
    assert_eq!(
        serde_json::to_value(&entries).unwrap(),
        json!([{ "id": settled.entry, "conversationId": root.id, "kind": "note", "data": { "text": "x" } }])
    );
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    assert!(conversation_tasks(&harness, &root).await.is_empty());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_idle_input_and_rejects_busy_input_with_when_busy_reject_without_writing() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = setup();
    *setup.now.lock().unwrap() = Some(42.0);
    let (step, reached) = unanswered();
    setup.faux.set_responses([step]);
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    reached.notified().await;
    let record = submission.status(&ctx()).await.unwrap();
    assert_eq!(record.status, SubmissionStatus::Placed);
    let entry_id = record.entry.unwrap();
    let entry = root.commit(move |tx: Tx| async move { tx.entry(entry_id).await }, &ctx()).await.unwrap().unwrap();
    assert!(USER_ENTRY.is(Some(&entry)));
    let expected = vec![Message::User(UserMessage { content: UserContent::Text("hi".into()), timestamp: 42 })];
    assert_eq!(entry.model, Some(expected));
    let live = harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    let run = live.get("run").unwrap();
    assert_eq!(run.get("inputs").unwrap().to_serde(), json!([submission.id]));
    let task_id: TaskId = run.get("taskId").unwrap().deserialize_into().unwrap();
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().kind, "pi.generation");

    let commits = storage.commit_count();
    let draft = SubmissionDraft::input("again").with_when_busy(WhenBusy::Reject);
    let Err(error) = root.submit(draft, &ctx()).await else { panic!("expected ConversationBusy") };
    assert!(matches!(error, DurableError::ConversationBusy(id) if id == root.id));
    assert_eq!(storage.commit_count(), commits);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn deduplicates_request_ids_per_conversation_before_any_write() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = setup();
    let (step, reached) = unanswered();
    setup.faux.set_responses([step]);
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    let first = root.submit(SubmissionDraft::input("hi").with_request_id("r1"), &ctx()).await.unwrap();
    reached.notified().await;
    let commits = storage.commit_count();
    // Deduplication runs before the busy check.
    let again = root.submit(SubmissionDraft::input("different").with_request_id("r1"), &ctx()).await.unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(storage.commit_count(), commits);
    let write = SubmissionDraft::write(note(None)).with_request_id("r1");
    let error = root.submit(write, &ctx()).await.err().unwrap();
    assert!(error.to_string().contains("Request r1 already identifies a submission of type input"), "{error}");
    let status = first.status(&ctx()).await.unwrap();
    assert_eq!((status.request_id.as_deref(), status.status), (Some("r1"), SubmissionStatus::Placed));

    let other = harness.create_conversation(CreateOptions::default(), &ctx()).await.unwrap();
    let write = other.submit(SubmissionDraft::write(note(None)).with_request_id("r1"), &ctx()).await.unwrap();
    assert_ne!(write.id, first.id);
    let repeated = other.submit(SubmissionDraft::write(note(None)).with_request_id("r1"), &ctx()).await.unwrap();
    assert_eq!(repeated.id, write.id);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_abort_results_and_looks_submissions_up_by_conversation() {
    let setup = setup();
    let release = Deferred::default();
    let held = release.clone();
    setup.faux.set_responses([FauxResponseStep::async_factory(move |_, _, _, _| {
        let held = held.clone();
        Box::pin(async move {
            held.wait().await;
            Ok(faux_assistant_message("answer", FauxMessageOptions::default()))
        })
    })]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    assert_eq!(submission.abort(&ctx()).await.unwrap(), AbortSubmissionResult::AlreadyPlaced);
    let by_root = harness.abort_submission(submission.id, &ctx(), Some(root.id)).await.unwrap();
    assert_eq!(by_root, AbortSubmissionResult::AlreadyPlaced);
    let other = harness.create_conversation(CreateOptions::default(), &ctx()).await.unwrap();
    let by_other = harness.abort_submission(submission.id, &ctx(), Some(other.id)).await.unwrap();
    assert_eq!(by_other, AbortSubmissionResult::NotFound);
    let missing = SubmissionId(999_999);
    assert_eq!(harness.abort_submission(missing, &ctx(), None).await.unwrap(), AbortSubmissionResult::NotFound);
    assert!(harness.submission(missing, &ctx()).await.unwrap().is_none());

    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    assert_eq!(submission.abort(&ctx()).await.unwrap(), AbortSubmissionResult::Settled);
    assert_eq!(harness.abort_submission(submission.id, &ctx(), None).await.unwrap(), AbortSubmissionResult::Settled);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn cancels_only_a_wait_and_rejects_pending_waits_on_close() {
    let setup = setup();
    setup.faux.set_responses([unanswered().0]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let controller = AbortController::new();
    let cancelled = {
        let (submission, context) = (submission.clone(), with_abort_signal(&controller.signal(), &ctx()));
        tokio::spawn(async move { submission.wait(&context).await })
    };
    let pending = {
        let submission = submission.clone();
        tokio::spawn(async move { submission.wait(&ctx()).await })
    };
    tokio::task::yield_now().await;
    controller.abort(Some(ChordError::error("stop waiting")));
    let error = cancelled.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("stop waiting"), "{error}");
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Placed);
    harness.close(&ctx()).await.unwrap();
    let error = pending.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("Harness is closed"), "{error}");
}

#[tokio::test]
async fn rejects_a_wait_whose_submission_read_spans_the_start_of_close() {
    let (entered, release) = (Deferred::default(), Deferred::default());
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let storage = Arc::new(ControlledStorage::default());
    let (armed, signal, gate) = (hold.clone(), entered.clone(), release.clone());
    *storage.call_hook.lock().unwrap() = Some(Arc::new(move |call: support::StorageCall| {
        let held = call.method == "submission" && armed.load(std::sync::atomic::Ordering::SeqCst);
        let (entered, release) = (signal.clone(), gate.clone());
        Box::pin(async move {
            if held {
                entered.resolve();
                release.wait().await;
            }
            Ok(())
        })
    }));
    let setup = setup();
    setup.faux.set_responses([unanswered().0]);
    let (harness, root) = open_chat(storage, &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let waiting = tokio::spawn(async move { submission.wait(&ctx()).await });
    entered.wait().await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&ctx()).await });
    support::flush().await;
    release.resolve();
    let error = waiting.await.unwrap().err().unwrap();
    assert!(error.to_string().contains("Harness is closed"), "{error}");
    closing.await.unwrap().unwrap();
}

#[tokio::test]
async fn reacquires_a_submission_after_reopen_and_settles_it_durably() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite");
    let path = path.to_str().unwrap();
    let setup = setup();
    // The first process never answers; the reopened one does.
    let (step, reached) = unanswered();
    setup.faux.set_responses([step, faux_assistant_message("after reopen", FauxMessageOptions::default()).into()]);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), &setup).await;
    let id = root.submit(SubmissionDraft::input("hi").with_request_id("print"), &ctx()).await.unwrap().id;
    reached.notified().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), &setup).await;
    let submission = harness.submission(id, &ctx()).await.unwrap().unwrap();
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Placed);
    harness.resume().unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done, "{:?}", setup.reports());
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), &setup).await;
    let again = harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(again, settled);
    let resubmitted = root.submit(SubmissionDraft::input("hi").with_request_id("print"), &ctx()).await.unwrap();
    assert_eq!(resubmitted.id, id);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn enables_scheduling_when_a_caller_submits_or_waits() {
    let setup = setup();
    setup.faux.set_responses([faux_assistant_message("answer", FauxMessageOptions::default())]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    // No resume(): submitting asks for progress.
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &self::setup()).await;
    let root_id = root.id;
    let create = move |tx: Tx| async move {
        let generation = pi_durable::harness::generation::generation_task();
        tx.create_task(generation, Json::from(json!({})), pi_durable::tasks::TaskOptions::conversation(root_id)).await
    };
    let task_id = root.commit(create, &ctx()).await.unwrap();
    // A committed task alone does not start scheduling; waiting for it does.
    let pending = harness.get_task(task_id, &ctx()).await.unwrap().unwrap();
    assert_eq!(pending.state.status(), TaskStatus::Pending);
    let settled = harness.wait_for_task(task_id, &ctx()).await.unwrap();
    assert_eq!(settled.state.status(), TaskStatus::Terminal);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn settles_submissions_by_their_current_record_in_the_transaction() {
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup()).await;
    let conversation_id = root.id;
    let append = move |tx: Tx| async move { Ok(tx.append_entry(conversation_id, EntryDraft::new("note")).await?.id) };
    let entry = root.commit(append, &ctx()).await.unwrap();
    let create = |kind: SubmissionType, status: SubmissionStatus| SubmissionCreate {
        conversation_id,
        kind,
        request_id: None,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
        trace: Default::default(),
    };
    let make = |record: SubmissionCreate| {
        let harness = harness.clone();
        async move {
            let commit = harness.commit(move |tx| async move { Ok(tx.create_submission(record).await?.id) }, &ctx());
            commit.await.unwrap()
        }
    };
    let queued = make(create(SubmissionType::Input, SubmissionStatus::Queued)).await;
    let write = make(create(SubmissionType::Write, SubmissionStatus::Queued)).await;
    let answer = SubmissionSettlement::Done { answer: entry };
    for id in [queued, write] {
        let settlement = answer.clone();
        let error = root.commit(move |tx: Tx| async move { tx.settle_submission(id, settlement) }, &ctx()).await;
        assert!(error.err().unwrap().to_string().contains("is not a placed input"));
    }
    let missing = SubmissionSettlement::Unanswered { reason: "x".into(), detail: None };
    let settle = move |tx: Tx| async move { tx.settle_submission(SubmissionId(999_999), missing) };
    assert!(root.commit(settle, &ctx()).await.err().unwrap().to_string().contains("does not exist"));

    // A submission created earlier in the same commit settles; a second settlement leaves the first.
    let placed = SubmissionCreate { entry: Some(entry), ..create(SubmissionType::Input, SubmissionStatus::Placed) };
    let commit = move |tx: Tx| async move {
        let id = tx.create_submission(placed).await?.id;
        tx.settle_submission(id, answer)?;
        tx.settle_submission(id, SubmissionSettlement::Unanswered { reason: "late".into(), detail: None })?;
        Ok(id)
    };
    let placed = harness.commit(commit, &ctx()).await.unwrap();
    let status = harness.submission(placed, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.entry, status.answer), (SubmissionStatus::Done, Some(entry), Some(entry)));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn appends_and_reads_typed_entries_through_tokens() {
    #[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Counter {
        n: u32,
    }
    let counter_entry = Arc::new(define_entry::<Counter>("app.counter").unwrap());
    let marker_entry = Arc::new(define_entry::<()>("app.marker").unwrap());
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup()).await;
    let id = root.id;
    let (token, draft) = (counter_entry.clone(), pi_durable::entries::TypedEntryDraft::new(Counter { n: 1 }));
    let append = move |tx: Tx| async move { tx.append_typed_entry(&token, id, draft).await };
    let counter = root.commit(append, &ctx()).await.unwrap();
    assert_eq!(counter.data, Counter { n: 1 });
    let (token, draft) = (marker_entry.clone(), pi_durable::entries::TypedEntryDraft::new(()));
    let append = move |tx: Tx| async move { tx.append_typed_entry(&token, id, draft).await };
    assert_eq!(root.commit(append, &ctx()).await.unwrap().record.kind, "app.marker");
    let read = |token: Arc<pi_durable::entries::Entry<Counter>>, entry: EntryId| {
        let root = root.clone();
        async move { root.commit(move |tx: Tx| async move { tx.typed_entry(&token, entry).await }, &ctx()).await }
    };
    assert_eq!(
        read(counter_entry.clone(), counter.record.id).await.unwrap().map(|entry| entry.data),
        Some(Counter { n: 1 })
    );
    assert!(read(counter_entry.clone(), EntryId(999_999)).await.unwrap().is_none());
    let (token, entry) = (marker_entry.clone(), counter.record.id);
    let as_marker = move |tx: Tx| async move { tx.typed_entry(&token, entry).await };
    assert!(root.commit(as_marker, &ctx()).await.unwrap().is_none());
    assert_eq!([USER_ENTRY.kind(), ASSISTANT_ENTRY.kind()], ["pi.user", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}
