//! The remaining upstream "task scheduling" cases.

use std::sync::atomic::{AtomicUsize, Ordering};

use pi_durable::session::SessionEnd;

use super::phases::gated;
use super::support::task_support::open_tasks;
use super::support::{ControlledStorage, Gate};
use super::*;

/// Open a Harness over `storage` holding `tasks`, with its root conversation and its collected reports.
async fn open_with(
    storage: &Arc<ControlledStorage>,
    tasks: Vec<Task>,
) -> (Harness, Conversation, Arc<Mutex<Vec<DurableError>>>) {
    let opened = open_tasks(storage.clone(), tasks).await;
    let root = opened.harness.root(&context(), CreateOptions::default()).await.unwrap();
    (opened.harness, root, opened.reports)
}

fn messages(reports: &Mutex<Vec<DurableError>>) -> Vec<String> {
    reports.lock().unwrap().iter().map(ToString::to_string).collect()
}

fn failed_with_storage_error(error: DurableError) {
    assert_eq!(error.name(), "SessionFailed");
    assert_eq!(error.session_failure_cause().unwrap().to_string(), "disk gone");
}

#[tokio::test]
async fn fails_the_harness_on_a_failed_reservation_commit_reopening_runs_the_task() {
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let once = one_step("test.once", move |_, runtime, context| {
        counter.fetch_add(1, Ordering::SeqCst);
        async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await }
    });
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root, reports) = open_with(&storage, vec![once.clone()]).await;
    let id = start(&root, &once, false).await;
    // Waiting resumes scheduling, so the failure is armed first: the next commit is the reservation.
    storage.fail_next_commit(DurableError::error("disk gone"));
    let waiting = {
        let harness = harness.clone();
        tokio::spawn(async move { harness.wait_for_task(id, &context()).await })
    };
    harness.resume().unwrap();
    let SessionEnd::Failed(error) = harness.closed().await else { panic!("the Harness must fail") };
    assert_eq!(error.to_string(), "disk gone");
    assert_eq!(messages(&reports), ["disk gone"]);
    // The pending wait and every later call get SessionFailed with the storage error.
    failed_with_storage_error(waiting.await.unwrap().unwrap_err());
    failed_with_storage_error(harness.get_task(id, &context()).await.unwrap_err());
    let submitted = root.submit(SubmissionDraft::input("x"), &context()).await.err().unwrap();
    assert_eq!(submitted.name(), "SessionFailed");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    harness.close(&context()).await.unwrap();
    storage.reopen();
    let (reopened, _, _) = open_with(&storage, vec![once]).await;
    reopened.resume().unwrap();
    assert_eq!(outcome(&reopened, id).await["status"], "completed");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    reopened.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_the_harness_instead_of_running_a_task_again_when_its_fault_write_fails() {
    let runs = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(ControlledStorage::default());
    let (counter, store) = (runs.clone(), storage.clone());
    let throws = one_step("test.failed-fault", move |_, _, _| {
        // The next commit is the step's fault write.
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            store.fail_next_commit(DurableError::error("disk gone"));
        }
        async { Err(DurableError::error("boom")) }
    });
    let (harness, root, reports) = open_with(&storage, vec![throws.clone()]).await;
    let id = start(&root, &throws, false).await;
    harness.resume().unwrap();
    let SessionEnd::Failed(error) = harness.closed().await else { panic!("the Harness must fail") };
    assert_eq!(error.to_string(), "disk gone");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(messages(&reports), ["disk gone"]);
    harness.close(&context()).await.unwrap();

    // Reopening recovers the task as after a crash: still running, so its phase runs again.
    storage.reopen();
    let (reopened, _, _) = open_with(&storage, vec![throws]).await;
    reopened.resume().unwrap();
    assert_eq!(outcome(&reopened, id).await, json!({ "status": "faulted", "error": { "message": "boom" } }));
    assert_eq!(runs.load(Ordering::SeqCst), 2);
    reopened.close(&context()).await.unwrap();
}

#[tokio::test]
async fn pages_the_newest_tasks_first_without_scanning_older_ones() {
    let idle = one_step("test.idle", |_, _, _| async { Ok(()) });
    let (harness, root, _) = open_root(vec![idle.clone()]).await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(start(&root, &idle, false).await);
    }
    let query = TaskQuery { order: Some(ScanOrder::Descending), ..TaskQuery::default() };
    let first = root.commit(move |tx: Tx| async move { tx.scan_tasks(query, 2, None).await }, &context()).await;
    let first = first.unwrap();
    assert_eq!(first.items.iter().map(|task| task.id).collect::<Vec<_>>(), [ids[4], ids[3]]);
    let next = first.next.clone();
    let scan = move |tx: Tx| async move { tx.scan_tasks(TaskQuery::default(), 2, next).await };
    let second = root.commit(scan, &context()).await.unwrap();
    assert_eq!(second.items.iter().map(|task| task.id).collect::<Vec<_>>(), [ids[2], ids[1]]);
    harness.close(&context()).await.unwrap();
}

/// Hold the line with a blocker commit; the returned handle settles once the gate is released.
async fn blocker(storage: &ControlledStorage, root: &Conversation) -> (Gate, tokio::task::JoinHandle<()>) {
    let held = storage.hold_commits();
    let (root, id) = (root.clone(), root.id);
    let committing = tokio::spawn(async move {
        let blocker = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("blocker")).await };
        root.commit(blocker, &context()).await.unwrap();
    });
    held.entered().await;
    (held, committing)
}

fn spawn_wait<F>(wait: F) -> tokio::task::JoinHandle<String>
where
    F: Future<Output = DurableResult<()>> + Send + 'static,
{
    tokio::spawn(async move { wait.await.unwrap_err().to_string() })
}

#[tokio::test]
async fn rejects_task_waits_cancelled_or_closed_while_queued_on_the_line_and_pending_waits_on_close() {
    let gate = Deferred::default();
    let blocking = gated("test.wait-close", gate.clone());
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root, _) = open_with(&storage, vec![blocking.clone()]).await;
    let id = start(&root, &blocking, false).await;

    // Hold the line with a commit, then queue waits behind it.
    let (held, committing) = blocker(&storage, &root).await;
    let controller = AbortController::new();
    let cancelled_context = with_abort_signal(&controller.signal(), &context());
    let waiting = harness.clone();
    let cancelled = spawn_wait(async move { waiting.wait_for_task(id, &cancelled_context).await.map(drop) });
    tokio::task::yield_now().await;
    controller.abort(Some(ChordError::error("wait cancelled")));
    held.release();
    committing.await.unwrap();
    assert!(cancelled.await.unwrap().contains("wait cancelled"));

    let (task_waiter, idle_waiter, conversation) = (harness.clone(), harness.clone(), root.clone());
    let pending = spawn_wait(async move { task_waiter.wait_for_task(id, &context()).await.map(drop) });
    let idle = spawn_wait(async move { idle_waiter.wait_for_idle(&context()).await });
    let conversation_idle = spawn_wait(async move { conversation.wait_for_idle(&context()).await });
    tokio::time::sleep(Duration::from_millis(10)).await;
    let (held, committing) = blocker(&storage, &root).await;
    let queued = harness.clone();
    let closed_while_queued = spawn_wait(async move { queued.wait_for_task(id, &context()).await.map(drop) });
    tokio::task::yield_now().await;
    let closer = harness.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    tokio::task::yield_now().await;
    held.release();
    committing.await.unwrap();
    for waiter in [closed_while_queued, pending, idle, conversation_idle] {
        let error = waiter.await.unwrap();
        assert!(error.contains("closed"), "{error}");
    }
    gate.resolve();
    closing.await.unwrap().unwrap();
}
