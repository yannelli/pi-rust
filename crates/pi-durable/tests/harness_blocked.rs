//! Port of upstream `test/harness-blocked.test.ts`: the crash-loop guard, recovery at open, and blocked tasks in
//! the task graph.

mod support;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::{Context, Json};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::task_support::{aborted, aborted_with, completed, eventually, open_tasks_with, running};
use support::{Deferred, context, flush};

/// A Storage that can crash: once `kill()`ed, every later call hangs, so it writes nothing more, not even the
/// `pending` records a close writes for the tasks it stops. `fail_next_commit()` fails one commit, as a reset would.
struct CrashStorage {
    inner: Arc<dyn Storage>,
    dead: AtomicBool,
    fail_next: Mutex<Option<DurableError>>,
}

impl CrashStorage {
    fn new(inner: Arc<dyn Storage>) -> Arc<Self> {
        Arc::new(Self { inner, dead: AtomicBool::new(false), fail_next: Mutex::default() })
    }

    async fn alive(&self) {
        if self.dead.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait]
impl Storage for CrashStorage {
    async fn commit(&self, writes: &[StorageWrite], adopt: Adopt<'_>, context: &Context) -> DurableResult<Seq> {
        self.alive().await;
        if let Some(error) = self.fail_next.lock().unwrap().take() {
            return Err(error);
        }
        self.inner.commit(writes, adopt, context).await
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.alive().await;
        self.inner.mint_id().await
    }

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>> {
        self.alive().await;
        self.inner.conversation(id, context).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        self.alive().await;
        self.inner.scan_conversations(query, limit, cursor, context).await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>> {
        self.alive().await;
        self.inner.entry(id, context).await
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        self.alive().await;
        self.inner.visible_entry(conversation_id, id, context).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        self.alive().await;
        self.inner.find_latest_head_marker(conversation_id, at_or_before, context).await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        self.alive().await;
        self.inner.scan_entries(query, limit, cursor, context).await
    }

    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.alive().await;
        self.inner.task(id, context).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        self.alive().await;
        self.inner.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        self.alive().await;
        self.inner.submission(id, context).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        self.alive().await;
        self.inner.scan_submissions(query, limit, cursor, context).await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        self.alive().await;
        self.inner.submission_by_request(conversation_id, request_id, context).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        self.alive().await;
        self.inner.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.alive().await;
        self.inner.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        self.alive().await;
        self.inner.scan_documents(query, limit, cursor, context).await
    }

    async fn close(&self, context: &Context) -> DurableResult<()> {
        self.alive().await;
        self.inner.close(context).await
    }
}

/// The restart limit the host's settings return, read at every access.
type Limit = Arc<Mutex<Option<f64>>>;

fn limit(restart_limit: f64) -> Limit {
    Arc::new(Mutex::new(Some(restart_limit)))
}

struct Opened {
    harness: Harness,
    storage: Arc<CrashStorage>,
    wakes: Arc<Mutex<Vec<Wake>>>,
    infos: Arc<Mutex<Vec<ReportInfo>>>,
}

impl Opened {
    /// A crash: the storage hangs from now on, and the Harness never closes it.
    async fn kill(self) {
        self.storage.dead.store(true, Ordering::SeqCst);
        self.storage.inner.close(&context()).await.unwrap();
    }

    fn infos(&self) -> Vec<ReportInfo> {
        self.infos.lock().unwrap().clone()
    }

    fn last_wake(&self) -> Option<Wake> {
        self.wakes.lock().unwrap().last().copied()
    }
}

async fn open_with(path: &str, registry: &Arc<Registry>, limit: &Limit) -> Opened {
    let storage = CrashStorage::new(Arc::new(open_sqlite_storage(path).await.unwrap()));
    open_on(storage, registry, limit).await
}

async fn open_on(storage: Arc<CrashStorage>, registry: &Arc<Registry>, limit: &Limit) -> Opened {
    let wakes = Arc::new(Mutex::new(Vec::new()));
    let (sink, limit) = (wakes.clone(), limit.clone());
    let opened = open_tasks_with(storage.clone(), Vec::new(), registry.clone(), move |mut options| {
        options.settings = Some(Arc::new(move || {
            Ok(HarnessSettings { restart_limit: *limit.lock().unwrap(), ..HarnessSettings::default() })
        }));
        options.with_on_wake(move |wake| {
            sink.lock().unwrap().push(wake);
            Ok(())
        })
    })
    .await;
    Opened { harness: opened.harness, storage, wakes, infos: opened.infos }
}

fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

#[derive(Default)]
struct Loop {
    crashing: AtomicBool,
    started: AtomicUsize,
}

impl Loop {
    fn crashing() -> Arc<Self> {
        Arc::new(Self { crashing: AtomicBool::new(true), started: AtomicUsize::new(0) })
    }

    fn started(&self) -> usize {
        self.started.load(Ordering::SeqCst)
    }
}

/// A task that hangs in its phase while `crashing`, as one killed every time would, and completes otherwise.
fn crash_loop(state: Arc<Loop>) -> TaskDefinition {
    TaskDefinition::new("test.crash-loop", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
        .phase("run", move |_, runtime, context| {
            let state = state.clone();
            async move {
                state.started.fetch_add(1, Ordering::SeqCst);
                if state.crashing.load(Ordering::SeqCst) {
                    aborted(runtime.signal()).await;
                    return Ok(());
                }
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
            }
        })
        .abort(|_, _, _| async { Ok(()) })
}

fn installed(name: &str, task: &Task) -> Arc<Registry> {
    let registry = Arc::new(create_registry());
    registry.install(Extension::new(name).task(task.clone())).unwrap();
    registry
}

async fn create(harness: &Harness, task: &Task, options: impl FnOnce(ConversationId) -> TaskOptions) -> TaskId {
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let (task, options) = (task.clone(), options(root.id));
    let create = move |tx: Tx| async move { tx.create_task(task, Json::Null, options).await };
    root.commit(create, &context()).await.unwrap()
}

async fn restarts(harness: &Harness, id: TaskId) -> Option<u32> {
    harness.get_task(id, &context()).await.unwrap().unwrap().restarts
}

fn report_task(id: TaskId, kind: &str) -> ReportTask {
    ReportTask { id, kind: kind.into(), conversation_id: ROOT_CONVERSATION_ID }
}

async fn blocked_state(harness: &Harness, id: TaskId) -> TaskInspectionState {
    let inspection = harness.inspect(&context()).await.unwrap();
    inspection.tasks.into_iter().find(|task| task.record.id == id).unwrap().state
}

/// Watch the task graph; `blocked` reads one task's `blocked` field.
async fn watch_graph(harness: &Harness) -> TaskGraphWatch {
    let graph = harness.watch_task_graph(&context()).await.unwrap();
    graph.start(|pi_chord::WatchChange { .. }, _| async { Ok(()) }).unwrap();
    graph
}

fn blocked(graph: &TaskGraphWatch, id: TaskId) -> Value {
    Value::from(&graph.value())["tasks"][id.to_string()]["blocked"].clone()
}

#[tokio::test]
async fn keeps_the_hosts_wake_active_until_opens_recovery_pass_has_landed() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let held = define_task(
        TaskDefinition::new("test.held", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = installed("held", &held);
    let no_limit: Limit = Arc::default();
    let first = open_with(&path, &registry, &no_limit).await;
    let id = create(&first.harness, &held, TaskOptions::conversation).await;
    first.harness.close(&context()).await.unwrap();
    // A crash left the task holding its outcome: the next open's recovery pass makes it terminal.
    let raw = open_sqlite_storage(&path).await.unwrap();
    let record = raw.task(id, &context()).await.unwrap().unwrap();
    let outcome = TaskOutcome::Completed { result: Some(Json::Null) };
    let completing = TaskRecord { memos: None, state: TaskState::Completing { outcome }, ..record };
    raw.commit(&[StorageWrite::Task { value: completing }], &|_| {}, &context()).await.unwrap();
    raw.close(&context()).await.unwrap();

    // That pass's commit fails, as on a platform reset.
    let storage = CrashStorage::new(Arc::new(open_sqlite_storage(&path).await.unwrap()));
    *storage.fail_next.lock().unwrap() = Some(DurableError::error("reset"));
    let opened = open_on(storage, &registry, &no_limit).await;
    assert_eq!(opened.harness.closed().await.reason(), "failed");
    // The host was told work is underway, and never that it ended: its recovery alarm stays.
    flush().await;
    assert_eq!(*opened.wakes.lock().unwrap(), [Wake { active: true, at: None }]);
}

#[tokio::test]
async fn faults_an_extensions_task_cut_off_restart_limit_times_in_a_row_instead_of_running_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let state = Loop::crashing();
    let looping = define_task(crash_loop(state.clone()));
    let registry = installed("loop", &looping);
    let three = limit(3.0);
    let mut opened = open_with(&path, &registry, &three).await;
    let id = create(&opened.harness, &looping, TaskOptions::conversation).await;
    // Each open runs the phase, and each crash cuts it off: three restarts.
    for run in 1..=3 {
        opened.harness.resume().unwrap();
        eventually(|| async { state.started() == run }).await;
        opened.kill().await; // a crash: a close would not count
        opened = open_with(&path, &registry, &three).await;
    }
    let harness = opened.harness.clone();
    assert_eq!(restarts(&harness, id).await, Some(3));
    // Nothing is parked: the task is ready, and the host is asked to run, which ends it.
    assert_eq!(blocked_state(&harness, id).await, TaskInspectionState::Ready { migrates: false });
    assert!(harness.wake().unwrap().active);
    harness.resume().unwrap();
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    let message = format!("Task {id} (test.crash-loop) was cut off 3 times without progress");
    let expected =
        json!({ "status": "faulted", "error": { "message": message, "detail": { "reason": "restart_limit" } } });
    assert_eq!(outcome, expected);
    assert_eq!(state.started(), 3);
    eventually(|| async { !opened.infos().is_empty() }).await;
    let faulted = ReportInfo::TaskFaulted {
        task: report_task(id, "test.crash-loop"),
        extension: Some("loop".into()),
        reason: FaultReason::RestartLimit,
    };
    assert_eq!(opened.infos(), [faulted]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn counts_no_restart_for_a_task_a_close_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let state = Loop::crashing();
    let looping = define_task(crash_loop(state.clone()));
    let registry = installed("loop", &looping);
    let one = limit(1.0);
    let opened = open_with(&path, &registry, &one).await;
    let id = create(&opened.harness, &looping, TaskOptions::conversation).await;
    opened.harness.resume().unwrap();
    eventually(|| async { state.started() == 1 }).await;
    opened.harness.close(&context()).await.unwrap();
    let opened = open_with(&path, &registry, &one).await;
    let record = opened.harness.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!(record.status(), TaskStatus::Pending);
    assert_eq!(record.restarts, None);
    state.crashing.store(false, Ordering::SeqCst);
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Completed { .. })));
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_an_abort_mark_admitted_before_close_in_the_record_close_writes_back() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let state = Loop::crashing();
    let looping = define_task(crash_loop(state.clone()));
    let registry = installed("loop", &looping);
    let opened = open_with(&path, &registry, &Arc::default()).await;
    let harness = opened.harness.clone();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = create(&harness, &looping, TaskOptions::conversation).await;
    harness.resume().unwrap();
    eventually(|| async { state.started() == 1 }).await;
    // Hold the line; the abort mark is admitted behind it, then close begins.
    let gate = Arc::new(Deferred::default());
    let held = gate.clone();
    let holding = tokio::spawn(async move {
        root.commit(
            move |_| async move {
                held.wait().await;
                Ok(())
            },
            &context(),
        )
        .await
    });
    flush().await;
    let aborting = tokio::spawn({
        let harness = harness.clone();
        async move { harness.abort_task(id, &context()).await }
    });
    flush().await;
    let closing = tokio::spawn({
        let harness = harness.clone();
        async move { harness.close(&context()).await }
    });
    // The invocation ends while the line is held; its write-back waits behind the abort mark.
    flush().await;
    gate.resolve();
    holding.await.unwrap().unwrap();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    closing.await.unwrap().unwrap();
    let raw = open_sqlite_storage(&path).await.unwrap();
    let record = raw.task(id, &context()).await.unwrap().unwrap();
    assert!(record.abort_requested);
    assert_eq!(record.status(), TaskStatus::Pending);
    raw.close(&context()).await.unwrap();
}

#[tokio::test]
async fn faults_a_task_whose_abort_handler_is_cut_off_restart_limit_times() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let aborts = Arc::new(AtomicUsize::new(0));
    let counted = aborts.clone();
    let stuck = define_task(
        TaskDefinition::new("test.stuck-abort", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, runtime, _| async move {
                aborted(runtime.signal()).await;
                Ok(())
            })
            .abort(move |_, runtime, _| {
                counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            }),
    );
    let registry = installed("stuck", &stuck);
    let one = limit(1.0);
    let opened = open_with(&path, &registry, &one).await;
    let id = create(&opened.harness, &stuck, TaskOptions::conversation).await;
    opened.harness.resume().unwrap();
    let harness = opened.harness.clone();
    tokio::spawn(async move { harness.abort_task(id, &context()).await });
    eventually(|| async { aborts.load(Ordering::SeqCst) == 1 }).await;
    opened.kill().await; // a crash in the abort handler
    let opened = open_with(&path, &registry, &one).await;
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    let Some(TaskOutcome::Faulted { error }) = settled.state.outcome() else { panic!("{:?}", settled.state) };
    assert_eq!(error.detail.as_ref().map(Value::from), Some(json!({ "reason": "restart_limit" })));
    // The receipt keeps no restart count.
    assert_eq!(settled.restarts, None);
    assert_eq!(aborts.load(Ordering::SeqCst), 1);
    eventually(|| async { !opened.infos().is_empty() }).await;
    let faulted = ReportInfo::TaskFaulted {
        task: report_task(id, "test.stuck-abort"),
        extension: Some("stuck".into()),
        reason: FaultReason::RestartLimit,
    };
    assert_eq!(opened.infos(), [faulted]);
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reads_restart_limit_when_it_reserves_the_task_a_raised_one_runs_it_again() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let state = Loop::crashing();
    let looping = define_task(crash_loop(state.clone()));
    let registry = installed("loop", &looping);
    let settings = limit(1.0);
    let opened = open_with(&path, &registry, &settings).await;
    let id = create(&opened.harness, &looping, TaskOptions::conversation).await;
    opened.harness.resume().unwrap();
    eventually(|| async { state.started() == 1 }).await;
    opened.kill().await; // a crash: a close would not count
    let opened = open_with(&path, &registry, &settings).await;
    assert_eq!(restarts(&opened.harness, id).await, Some(1));
    state.crashing.store(false, Ordering::SeqCst);
    // The host's live settings change before the task is reserved; it runs instead of ending.
    *settings.lock().unwrap() = Some(f64::INFINITY);
    opened.harness.resume().unwrap();
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Completed { .. })));
    assert_eq!(state.started(), 2);
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn runs_the_abort_handler_of_a_task_at_the_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let state = Loop::crashing();
    let aborted_ids = Arc::new(Mutex::new(Vec::new()));
    let recorded = aborted_ids.clone();
    let looping = define_task(crash_loop(state.clone()).abort(move |task, runtime, context| {
        recorded.lock().unwrap().push(task.id);
        async move { runtime.commit(|_, _| async { Ok(aborted_with("test")) }, &context).await }
    }));
    let registry = installed("loop", &looping);
    let one = limit(1.0);
    let opened = open_with(&path, &registry, &one).await;
    let id = create(&opened.harness, &looping, TaskOptions::conversation).await;
    opened.harness.resume().unwrap();
    eventually(|| async { state.started() == 1 }).await;
    opened.kill().await; // a crash: a close would not count
    let opened = open_with(&path, &registry, &one).await;
    opened.harness.abort_task(id, &context()).await.unwrap();
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Aborted { .. })));
    assert_eq!(*aborted_ids.lock().unwrap(), [id]);
    assert_eq!(state.started(), 1);
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_count_the_same_checkpoint_committed_again_as_progress() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let started = Arc::new(AtomicUsize::new(0));
    let counted = started.clone();
    let intent = define_task(
        TaskDefinition::new("test.intent", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |task, runtime, context| {
                counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    // Intent before an effect, then the effect never returns: the checkpoint is unchanged.
                    let checkpoint = task.state.checkpoint().cloned().unwrap_or_default();
                    let next = Some(TaskState::Running { checkpoint });
                    runtime.commit(move |_, _| async move { Ok(next) }, &context).await?;
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = installed("intent", &intent);
    let two = limit(2.0);
    let mut opened = open_with(&path, &registry, &two).await;
    let id = create(&opened.harness, &intent, TaskOptions::conversation).await;
    for run in 1..=2 {
        opened.harness.resume().unwrap();
        eventually(|| async { started.load(Ordering::SeqCst) == run }).await;
        let harness = opened.harness.clone();
        eventually(|| async {
            harness.get_task(id, &context()).await.unwrap().unwrap().status() == TaskStatus::Running
        })
        .await;
        opened.kill().await; // a crash: a close would not count
        opened = open_with(&path, &registry, &two).await;
    }
    assert_eq!(restarts(&opened.harness, id).await, Some(2));
    opened.harness.resume().unwrap();
    let settled = opened.harness.wait_for_task(id, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!(
        (&outcome["status"], &outcome["error"]["detail"]),
        (&json!("faulted"), &json!({ "reason": "restart_limit" }))
    );
    assert_eq!(started.load(Ordering::SeqCst), 2);
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn starts_the_count_over_when_the_task_makes_progress() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let crashing = Arc::new(AtomicBool::new(true));
    let started = Arc::new(AtomicUsize::new(0));
    let (crash, one_started, two_started) = (crashing.clone(), started.clone(), started.clone());
    let steps = define_task(
        TaskDefinition::new("test.steps", 1, |_| Ok(Json::from(json!({ "phase": "one" }))))
            .phase("one", move |_, runtime, context| {
                one_started.fetch_add(1, Ordering::SeqCst);
                let crashing = crash.load(Ordering::SeqCst);
                async move {
                    if crashing {
                        aborted(runtime.signal()).await;
                        return Ok(());
                    }
                    runtime.commit(|_, _| async { Ok(running(json!({ "phase": "two" }))) }, &context).await
                }
            })
            .phase("two", move |_, runtime, _| {
                two_started.fetch_add(1, Ordering::SeqCst);
                async move {
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = installed("steps", &steps);
    let two = limit(2.0);
    let opened = open_with(&path, &registry, &two).await;
    let id = create(&opened.harness, &steps, TaskOptions::conversation).await;
    opened.harness.resume().unwrap();
    eventually(|| async { started.load(Ordering::SeqCst) == 1 }).await;
    opened.kill().await; // a crash: a close would not count
    let opened = open_with(&path, &registry, &two).await;
    assert_eq!(restarts(&opened.harness, id).await, Some(1));
    // Progress: phase one commits phase two, which resets the count.
    crashing.store(false, Ordering::SeqCst);
    opened.harness.resume().unwrap();
    eventually(|| async { started.load(Ordering::SeqCst) == 3 }).await;
    assert_eq!(restarts(&opened.harness, id).await, None);
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn tells_the_graph_and_the_host_when_a_migration_fails() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let definition = |version: u32| {
        TaskDefinition::new("test.migrating", version, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) })
    };
    let v1 = define_task(definition(1));
    let v2 = define_task(definition(2).migrate(|_, _, _| Err(DurableError::error("cannot migrate"))));
    let no_limit: Limit = Arc::default();
    let opened = open_with(&path, &installed("v1", &v1), &no_limit).await;
    // Created, never run: scheduling stays off.
    let id = create(&opened.harness, &v1, TaskOptions::conversation).await;
    opened.harness.close(&context()).await.unwrap();
    let opened = open_with(&path, &installed("v2", &v2), &no_limit).await;
    let graph = watch_graph(&opened.harness).await;
    opened.harness.resume().unwrap();
    eventually(|| async { blocked(&graph, id) == json!("migration_failed") }).await;
    eventually(|| async { !opened.harness.wake().unwrap().active }).await;
    assert_eq!(opened.harness.wake().unwrap(), Wake { active: false, at: None });
    eventually(|| async { opened.last_wake().is_some_and(|wake| !wake.active) }).await;
    graph.stop().await;
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn shows_an_abandoned_task_waiting_for_its_definition_as_blocked() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let abandoned = define_task(
        TaskDefinition::new("test.abandoned", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let no_limit: Limit = Arc::default();
    let opened = open_with(&path, &installed("abandoned", &abandoned), &no_limit).await;
    let options = |id| TaskOptions { abandon_on_restart: true, ..TaskOptions::conversation(id) };
    let id = create(&opened.harness, &abandoned, options).await;
    opened.harness.close(&context()).await.unwrap();
    // The next open marks it `restart`; without its definition, it waits for one to clean up.
    let opened = open_with(&path, &Arc::new(create_registry()), &no_limit).await;
    let graph = watch_graph(&opened.harness).await;
    opened.harness.resume().unwrap();
    let harness = opened.harness.clone();
    eventually(|| async {
        harness.get_task(id, &context()).await.unwrap().unwrap().abort_reason == Some(AbortReason::Restart)
    })
    .await;
    eventually(|| async { blocked(&graph, id) == json!("missing_task") }).await;
    graph.stop().await;
    opened.harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn shows_a_task_without_a_definition_as_blocked_and_wakes_the_host_once_a_registry_change_unblocks_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let missing = define_task(
        TaskDefinition::new("test.missing", 1, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = Arc::new(create_registry());
    let opened = open_with(&path, &registry, &Arc::default()).await;
    let harness = opened.harness.clone();
    let graph = watch_graph(&harness).await;
    let id = create(&harness, &missing, TaskOptions::conversation).await;
    harness.resume().unwrap();
    eventually(|| async { blocked(&graph, id) == json!("missing_task") }).await;
    eventually(|| async { !harness.wake().unwrap().active }).await;
    opened.wakes.lock().unwrap().clear();
    registry.install(Extension::new("late").task(missing.clone())).unwrap();
    // The graph follows the registry; the host hears it can run work now.
    eventually(|| async { blocked(&graph, id).is_null() }).await;
    eventually(|| async { opened.wakes.lock().unwrap().iter().any(|wake| wake.active) }).await;
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Completed { .. })));
    graph.stop().await;
    harness.close(&context()).await.unwrap();
}
