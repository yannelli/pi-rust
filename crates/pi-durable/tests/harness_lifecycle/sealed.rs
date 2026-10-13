//! Operations after close begins, scheduling on a paused Harness, and registry changes before resume.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;
use pi_chord::AbortController;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::*;

use super::support::task_support::{eventually, open_tasks};
use super::support::{ControlledStorage, context, flush};
use super::{ChordError, notes_doc, one_step, runner, set_text, start, with_signal};

type Operation = BoxFuture<'static, DurableResult<String>>;

/// "closed" for an error that says so, the error text otherwise, and `resolved` or `undefined` for a value.
async fn outcome(operation: Operation) -> String {
    match operation.await {
        Ok(value) => value,
        Err(error) if error.to_string().contains("closed") => "closed".into(),
        Err(error) => error.to_string(),
    }
}

fn op<T: Send + 'static>(future: impl Future<Output = DurableResult<T>> + Send + 'static) -> Operation {
    Box::pin(async move { future.await.map(|_| "resolved".to_owned()) })
}

fn optional<T: Send + 'static>(future: impl Future<Output = DurableResult<Option<T>>> + Send + 'static) -> Operation {
    Box::pin(async move { future.await.map(|value| if value.is_some() { "resolved" } else { "undefined" }.to_owned()) })
}

async fn hold_line(
    storage: &Arc<ControlledStorage>,
    harness: &Harness,
    root: &Conversation,
) -> (super::support::Gate, tokio::task::JoinHandle<DurableResult<()>>) {
    let held = storage.hold_commits();
    let (blocking_harness, id) = (harness.clone(), root.id);
    let blocking = tokio::spawn(async move {
        let append = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("blocker")).await.map(|_| ()) };
        blocking_harness.commit(append, &context()).await
    });
    held.entered().await;
    (held, blocking)
}

#[tokio::test]
async fn leaves_no_mount_behind_a_watch_acquisition_cancelled_on_the_line() {
    // Upstream also counts the commit subscriptions of document observers by patching `subscribeCommits`; a Rust
    // Session's methods cannot be replaced, so this checks the rejections and that no view mount was kept.
    let notes = notes_doc("test.cancelled-watch");
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    set_text(&harness, &notes, "x").await;

    // Cancel a document watch while its acquisition loads the document on the line.
    harness.unload_documents().await.unwrap();
    let find = storage.hold_find_document();
    let controller = AbortController::new();
    let (watching, doc, watch_context) = (harness.clone(), notes.clone(), with_signal(&controller));
    let doc_watch = tokio::spawn(async move { watching.watch_doc(&doc, (), &watch_context).await.map(|_| ()) });
    find.entered().await;
    controller.abort(Some(ChordError::error("cancelled")));
    find.release();
    assert!(doc_watch.await.unwrap().unwrap_err().to_string().contains("cancelled"));

    // Cancel a view watch while it builds its mount on the line.
    harness.unload_documents().await.unwrap();
    let find = storage.hold_find_document();
    let controller = AbortController::new();
    let (watching, watch_context) = (root.clone(), with_signal(&controller));
    let view_watch = tokio::spawn(async move { watching.watch(&watch_context).await.map(|_| ()) });
    find.entered().await;
    controller.abort(Some(ChordError::error("cancelled")));
    find.release();
    assert!(view_watch.await.unwrap().unwrap_err().to_string().contains("cancelled"));
    // No observer kept the mount: the next observer builds a new one, and the one after that another.
    let first = root.view_state(&context()).await.unwrap();
    let value = first.value();
    first.dispose().unwrap();
    let second = root.view_state(&context()).await.unwrap();
    assert!(!second.value().same_ref(&value));
    second.dispose().unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_conversation_and_harness_operations_once_close_begins_inspect_queued_before_reports_closing() {
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let append = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("note")).await };
    let entry = root.commit(append, &context()).await.unwrap();
    let submission = root.submit(SubmissionDraft::write(EntryDraft::new("note")), &context()).await.unwrap();

    let (held, blocking) = hold_line(&storage, &harness, &root).await;
    let inspecting = harness.clone();
    let queued_inspect = tokio::spawn(async move { inspecting.inspect(&context()).await });
    flush().await;
    let close_harness = harness.clone();
    let closing = tokio::spawn(async move { close_harness.close(&context()).await });
    flush().await;
    let (h, r, s, c) = (harness.clone(), root.clone(), submission.clone(), context());
    let thinking = AgentChange { thinking_level: Change::Set(pi_ai::ThinkingLevel::Low), ..AgentChange::default() };
    let ownerless = || CreateOptions { ownership: ConversationOwnership::Ownerless, ..CreateOptions::default() };
    let operations: Vec<(&str, Operation)> = vec![
        (
            "submit",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.submit(SubmissionDraft::input("x"), &c).await }
            }),
        ),
        (
            "agent",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.agent(&c).await }
            }),
        ),
        (
            "configure",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.configure(thinking, &c).await }
            }),
        ),
        (
            "commit",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.commit(|_| async { Ok(()) }, &c).await }
            }),
        ),
        (
            "context",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.context(&c, None).await }
            }),
        ),
        (
            "entries",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.entries(EntryRange::default(), 10, None, &c).await }
            }),
        ),
        (
            "fork",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.fork(entry.id, ownerless(), &c).await }
            }),
        ),
        (
            "compact",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.compact(None, &c).await }
            }),
        ),
        (
            "reset",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.reset(None, &c).await }
            }),
        ),
        (
            "abort",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.abort(&c, false).await }
            }),
        ),
        (
            "conversationIdle",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.wait_for_idle(&c).await }
            }),
        ),
        (
            "viewState",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.view_state(&c).await }
            }),
        ),
        (
            "watch",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.watch(&c).await }
            }),
        ),
        (
            "taskGraph",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.task_graph(&c).await }
            }),
        ),
        (
            "watchTaskGraph",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.watch_task_graph(&c).await }
            }),
        ),
        (
            "status",
            op({
                let (s, c) = (s.clone(), c.clone());
                async move { s.status(&c).await }
            }),
        ),
        (
            "wait",
            op({
                let (s, c) = (s.clone(), c.clone());
                async move { s.wait(&c).await }
            }),
        ),
        (
            "abortSubmission",
            op({
                let (s, c) = (s.clone(), c.clone());
                async move { s.abort(&c).await }
            }),
        ),
        (
            "root",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.root(&c, CreateOptions::default()).await }
            }),
        ),
        (
            "conversation",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.conversation(id, &c).await }
            }),
        ),
        (
            "createConversation",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.create_conversation(ownerless(), &c).await }
            }),
        ),
        (
            "getTask",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.get_task(TaskId(1), &c).await }
            }),
        ),
        (
            "inspect",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.inspect(&c).await }
            }),
        ),
        (
            "submission",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.submission(s.id, &c).await }
            }),
        ),
        (
            "abortTask",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.abort_task(TaskId(1), &c).await }
            }),
        ),
        (
            "waitForTask",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.wait_for_task(TaskId(1), &c).await }
            }),
        ),
        (
            "harnessIdle",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.wait_for_idle(&c).await }
            }),
        ),
        (
            "usage",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.usage(&c).await }
            }),
        ),
    ];
    let mut outcomes = BTreeMap::new();
    for (name, operation) in operations {
        outcomes.insert(name, outcome(operation).await);
    }
    let expected: BTreeMap<&str, String> = outcomes.keys().map(|name| (*name, "closed".to_owned())).collect();
    assert_eq!(outcomes, expected);
    assert!(harness.resume().unwrap_err().to_string().contains("closed"));
    held.release();
    blocking.await.unwrap().unwrap();
    assert_eq!(queued_inspect.await.unwrap().unwrap().scheduling, Scheduling::Closing);
    closing.await.unwrap().unwrap();
}

#[tokio::test]
async fn completes_reads_queued_at_the_seal_and_rejects_queued_waits_and_acquisitions_that_would_follow_commits() {
    let (notes, absent) = (notes_doc("test.queued-at-seal"), notes_doc("test.queued-absent"));
    let pending = one_step("test.queued-pending", None);
    let storage = Arc::new(ControlledStorage::default());
    let harness = open_tasks(storage.clone(), Vec::new()).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    set_text(&harness, &notes, "").await;
    let task_id = start(&root, &pending, false).await;
    let write = root.submit(SubmissionDraft::write(EntryDraft::new("note")), &context()).await.unwrap();
    write.wait(&context()).await.unwrap();

    let (held, blocking) = hold_line(&storage, &harness, &root).await;
    let (h, r, c) = (harness.clone(), root.clone(), context());
    let queued: Vec<(&str, tokio::task::JoinHandle<String>)> = vec![
        (
            "context",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.context(&c, None).await }
            }),
        ),
        (
            "snapshot",
            op({
                let (h, c, d) = (h.clone(), c.clone(), notes.clone());
                async move { h.snapshot(&d, (), &c).await }
            }),
        ),
        (
            "settledWait",
            op({
                let (w, c) = (write.clone(), c.clone());
                async move { w.wait(&c).await }
            }),
        ),
        (
            "absentWatch",
            optional({
                let (h, c, d) = (h.clone(), c.clone(), absent.clone());
                async move { h.watch_doc(&d, (), &c).await }
            }),
        ),
        (
            "absentState",
            optional({
                let (h, c, d) = (h.clone(), c.clone(), absent.clone());
                async move { h.document_state(&d, (), &c).await }
            }),
        ),
        (
            "waitForTask",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.wait_for_task(task_id, &c).await }
            }),
        ),
        (
            "documentState",
            op({
                let (h, c, d) = (h.clone(), c.clone(), notes.clone());
                async move { h.document_state(&d, (), &c).await }
            }),
        ),
        (
            "watchDoc",
            op({
                let (h, c, d) = (h.clone(), c.clone(), notes.clone());
                async move { h.watch_doc(&d, (), &c).await }
            }),
        ),
        (
            "viewState",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.view_state(&c).await }
            }),
        ),
        (
            "watch",
            op({
                let (r, c) = (r.clone(), c.clone());
                async move { r.watch(&c).await }
            }),
        ),
        (
            "taskGraph",
            op({
                let (h, c) = (h.clone(), c.clone());
                async move { h.task_graph(&c).await }
            }),
        ),
    ]
    .into_iter()
    .map(|(name, operation)| (name, tokio::spawn(outcome(operation))))
    .collect();
    flush().await;
    let close_harness = harness.clone();
    let closing = tokio::spawn(async move { close_harness.close(&context()).await });
    flush().await;
    held.release();
    blocking.await.unwrap().unwrap();
    let mut outcomes = BTreeMap::new();
    for (name, handle) in queued {
        outcomes.insert(name, handle.await.unwrap());
    }
    let expected = [
        ("context", "resolved"),
        ("snapshot", "resolved"),
        ("settledWait", "resolved"),
        ("absentWatch", "undefined"),
        ("absentState", "undefined"),
        ("waitForTask", "closed"),
        ("documentState", "closed"),
        ("watchDoc", "closed"),
        ("viewState", "closed"),
        ("watch", "closed"),
        ("taskGraph", "closed"),
    ];
    let expected: BTreeMap<&str, String> = expected.into_iter().map(|(name, value)| (name, value.to_owned())).collect();
    assert_eq!(outcomes, expected);
    closing.await.unwrap().unwrap();
}

struct Paused {
    harness: Harness,
    root: Conversation,
    id: TaskId,
    submission_id: SubmissionId,
    ran: Arc<AtomicBool>,
}

/// Open a paused Harness with one background task pending and a queued write submission.
async fn paused() -> Paused {
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let marker = one_step(
        "test.marker",
        runner(move || {
            flag.store(true, Ordering::SeqCst);
            async {}
        }),
    );
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![marker.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    // Background, so idle waits and conversation abort leave it alone.
    let id = start(&root, &marker, true).await;
    let conversation_id = root.id;
    let create = move |tx: Tx| async move {
        Ok(tx.create_submission(SubmissionCreate::queued(conversation_id, SubmissionType::Write, None)).await?.id)
    };
    let submission_id = harness.commit(create, &context()).await.unwrap();
    Paused { harness, root, id, submission_id, ran }
}

#[tokio::test]
async fn never_schedules_from_a_read_only_viewer() {
    let Paused { harness, root, id, submission_id, ran } = paused().await;
    let notes = notes_doc("test.viewer-notes");
    set_text(&harness, &notes, "").await;
    let c = context();
    harness.inspect(&c).await.unwrap();
    harness.get_task(id, &c).await.unwrap();
    harness.usage(&c).await.unwrap();
    harness.conversation(root.id, &c).await.unwrap();
    harness.snapshot(&notes, (), &c).await.unwrap();
    harness.document_state(&notes, (), &c).await.unwrap().unwrap().dispose().unwrap();
    harness.watch_doc(&notes, (), &c).await.unwrap().unwrap().stop().await;
    harness.submission(submission_id, &c).await.unwrap().unwrap().status(&c).await.unwrap();
    root.agent(&c).await.unwrap();
    root.context(&c, None).await.unwrap();
    root.entries(EntryRange::default(), 10, None, &c).await.unwrap();
    root.view_state(&c).await.unwrap().dispose().unwrap();
    root.watch(&c).await.unwrap().stop().await;
    harness.task_graph(&c).await.unwrap().dispose().unwrap();
    harness.watch_task_graph(&c).await.unwrap().stop().await;
    flush().await;
    flush().await;
    assert!(!ran.load(Ordering::SeqCst));
    assert_eq!(harness.inspect(&c).await.unwrap().scheduling, Scheduling::Paused);
    harness.close(&c).await.unwrap();
}

#[tokio::test]
async fn schedules_from_every_progress_call() {
    let names = ["submit", "compact", "abort", "conversationIdle", "submissionWait", "waitForTask", "harnessIdle"];
    for name in names {
        let opened = paused().await;
        let (harness, root, c) = (opened.harness.clone(), opened.root.clone(), context());
        let (id, submission_id) = (opened.id, opened.submission_id);
        let call: Operation = match name {
            "submit" => op(async move { root.submit(SubmissionDraft::write(EntryDraft::new("note")), &c).await }),
            "compact" => op(async move { root.compact(None, &c).await }),
            "abort" => op(async move { root.abort(&c, false).await }),
            "conversationIdle" => op(async move { root.wait_for_idle(&c).await }),
            "submissionWait" => op(async move { harness.submission(submission_id, &c).await?.unwrap().wait(&c).await }),
            "waitForTask" => op(async move { harness.wait_for_task(id, &c).await }),
            _ => op(async move { harness.wait_for_idle(&c).await }),
        };
        // A queued write's wait settles only on placement; close rejects it.
        let pending = tokio::spawn(call);
        let ran = opened.ran.clone();
        eventually(|| {
            let ran = ran.load(Ordering::SeqCst);
            async move { ran }
        })
        .await;
        opened.harness.close(&context()).await.unwrap();
        let _ = pending.await;
    }
}

#[tokio::test]
async fn runs_what_the_registry_holds_at_resume_a_definition_installed_or_replaced_after_open() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let define = |label: &'static str| {
        let sink = log.clone();
        one_step(
            "test.late-definition",
            runner(move || {
                sink.lock().unwrap().push(label);
                async {}
            }),
        )
    };
    let opened = open_tasks(Arc::new(MemoryStorage::new()), Vec::new()).await;
    let harness = opened.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let missing = start(&root, &define("unused"), false).await;
    let states =
        |inspection: HarnessInspection| inspection.tasks.into_iter().map(|task| task.state).collect::<Vec<_>>();
    let blocked = states(harness.inspect(&context()).await.unwrap());
    assert!(
        matches!(blocked[..], [TaskInspectionState::Blocked { reason: BlockedReason::MissingTask, .. }]),
        "{blocked:?}"
    );
    let install = |task: Task| opened.registry.install(Extension::new("task:test.late-definition").task(task)).unwrap();
    install(define("v1"));
    let ready = states(harness.inspect(&context()).await.unwrap());
    assert!(matches!(ready[..], [TaskInspectionState::Ready { .. }]), "{ready:?}");
    // The same extension name replaces the definition in place, still before resume.
    opened.registry.uninstall("task:test.late-definition").unwrap();
    install(define("v2"));
    harness.resume().unwrap();
    harness.wait_for_task(missing, &context()).await.unwrap();
    assert_eq!(*log.lock().unwrap(), ["v2"]);
    harness.close(&context()).await.unwrap();
}
