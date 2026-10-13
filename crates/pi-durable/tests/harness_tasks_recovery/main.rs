//! Port of upstream `test/harness-tasks-recovery.test.ts`. This file holds the helpers and the recovery and crash
//! cases; `blocked.rs` holds blocked tasks, and `handover.rs` definition handover and Harness open.

#[path = "../support/mod.rs"]
mod support;

mod blocked;
mod handover;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};
use support::task_support::{aborted, aborted_with, completed, eventually, open_tasks, open_tasks_with};
use support::{ControlledStorage, Deferred, context, flush};

pub type Log = Arc<Mutex<Vec<String>>>;

pub fn log_of(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

pub fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

pub async fn sqlite(path: &str) -> Arc<dyn Storage> {
    Arc::new(open_sqlite_storage(path).await.unwrap())
}

pub async fn create_in(harness: &Harness, task: &Task, input: Value) -> TaskId {
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let (task, id) = (task.clone(), root.id);
    let create =
        move |tx: Tx| async move { tx.create_task(task, Json::from(input), TaskOptions::conversation(id)).await };
    root.commit(create, &context()).await.unwrap()
}

pub fn outcome(record: &TaskRecord) -> Value {
    serde_json::to_value(record.state.outcome()).unwrap()
}

pub fn step() -> Json {
    Json::from(json!({ "phase": "run" }))
}

/// Fake external service whose operations are idempotent by request key.
#[derive(Default)]
struct TransferService {
    applied: Mutex<Vec<(String, u64)>>,
    calls: AtomicUsize,
}

impl TransferService {
    fn apply(&self, key: &str, amount: u64) -> u64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut applied = self.applied.lock().unwrap();
        if let Some((_, receipt)) = applied.iter().find(|(other, _)| other == key) {
            return *receipt;
        }
        applied.push((key.to_owned(), amount * 10));
        amount * 10
    }
}

/// Intent/effect/outcome task: `prepare` commits the intent, `apply` does the effect and commits the outcome.
/// While `interrupt` is set, the first `apply` blocks after the effect until the invocation is signalled.
fn transfer_task(service: Arc<TransferService>, interrupt: Arc<AtomicBool>) -> Task {
    define_task(
        TaskDefinition::new("test.transfer", 1, |_| Ok(Json::from(json!({ "phase": "prepare" }))))
            .phase("prepare", |task, runtime, context| async move {
                runtime.memo_or("requested", task.input["amount"].clone(), &context).await?;
                let key = format!("transfer-{}", task.id);
                let next = Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "apply", "key": key })) });
                runtime.commit(move |_, _| async move { Ok(next) }, &context).await
            })
            .phase("apply", move |task, runtime, context| {
                let key = task.state.checkpoint().and_then(|checkpoint| checkpoint["key"].as_str().map(str::to_owned));
                let receipt = service.apply(&key.unwrap_or_default(), task.input["amount"].as_u64().unwrap());
                let interrupt = interrupt.swap(false, Ordering::SeqCst);
                async move {
                    if interrupt {
                        aborted(runtime.signal()).await;
                    }
                    runtime
                        .commit(move |_, _| async move { Ok(completed(json!({ "receipt": receipt }))) }, &context)
                        .await
                }
            })
            .abort(|_, runtime, context| async move {
                let state = TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } };
                runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
            }),
    )
}

#[tokio::test]
async fn resumes_an_intent_effect_outcome_task_interrupted_after_its_intent_across_close_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let service = Arc::new(TransferService::default());
    let transfer = transfer_task(service.clone(), Arc::new(AtomicBool::new(true)));

    let first = open_tasks(sqlite(&path).await, vec![transfer.clone()]).await.harness;
    let id = create_in(&first, &transfer, json!({ "amount": 7 })).await;
    first.resume().unwrap();
    eventually(|| async { service.calls.load(Ordering::SeqCst) == 1 }).await;
    first.close(&context()).await.unwrap();

    let second = open_tasks(sqlite(&path).await, vec![transfer.clone()]).await.harness;
    // Open reconciled `running` to `pending` and kept the checkpoint and memos; nothing ran yet.
    let reopened = serde_json::to_value(second.get_task(id, &context()).await.unwrap().unwrap()).unwrap();
    assert_eq!(
        (reopened["abortRequested"].clone(), reopened["memos"].clone()),
        (json!(false), json!({ "requested": 7 }))
    );
    let expected = json!({ "status": "pending", "checkpoint": { "phase": "apply", "key": format!("transfer-{id}") } });
    assert_eq!(reopened["state"], expected);
    assert_eq!(service.calls.load(Ordering::SeqCst), 1);
    second.resume().unwrap();
    let receipt = second.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&receipt), json!({ "status": "completed", "result": { "receipt": 70 } }));
    assert_eq!(service.calls.load(Ordering::SeqCst), 2);
    assert_eq!(service.applied.lock().unwrap().len(), 1);
    second.close(&context()).await.unwrap();

    let third = open_tasks(sqlite(&path).await, vec![transfer]).await.harness;
    assert_eq!(third.get_task(id, &context()).await.unwrap(), Some(receipt));
    third.close(&context()).await.unwrap();
}

// Upstream #10549.
#[tokio::test]
async fn keeps_a_task_s_started_at_across_close_and_reopen_and_stamps_ended_at_when_it_settles() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let service = Arc::new(TransferService::default());
    let transfer = transfer_task(service.clone(), Arc::new(AtomicBool::new(true)));
    let clock = Arc::new(Mutex::new(2_000.0));
    let now = |clock: &Arc<Mutex<f64>>| {
        let clock = clock.clone();
        move |options: HarnessOptions| options.with_now(move || *clock.lock().unwrap())
    };
    let registry = || Arc::new(create_registry());
    let first = open_tasks_with(sqlite(&path).await, vec![transfer.clone()], registry(), now(&clock)).await.harness;
    let id = create_in(&first, &transfer, json!({ "amount": 7 })).await;
    first.resume().unwrap();
    eventually(|| async { service.calls.load(Ordering::SeqCst) == 1 }).await;
    *clock.lock().unwrap() = 3_000.0;
    first.close(&context()).await.unwrap();

    *clock.lock().unwrap() = 9_000.0;
    let second = open_tasks_with(sqlite(&path).await, vec![transfer], registry(), now(&clock)).await.harness;
    let reopened = second.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!(
        (reopened.state.status(), reopened.started_at, reopened.ended_at),
        (TaskStatus::Pending, Some(2_000.0), None)
    );
    second.resume().unwrap();
    let receipt = second.wait_for_task(id, &context()).await.unwrap();
    assert_eq!((receipt.started_at, receipt.ended_at), (Some(2_000.0), Some(9_000.0)));
    second.close(&context()).await.unwrap();
}

#[tokio::test]
async fn resumes_abort_work_after_close_at_every_direct_task_abort_stage() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let log = Log::default();
    let block = Arc::new(AtomicBool::new(true));
    let (abort_reached, run_release) = (Deferred::default(), Deferred::default());
    let (run_log, release) = (log.clone(), run_release.clone());
    let (abort_log, blocking, reached) = (log.clone(), block.clone(), abort_reached.clone());
    let abortable = define_task(
        TaskDefinition::new("test.abortable", 1, |_| Ok(step()))
            .phase("run", move |_, _, _| {
                run_log.lock().unwrap().push("run".into());
                let release = release.clone();
                // Ignores the abort signal until released, so the mark is durable while the run is active.
                async move {
                    release.wait().await;
                    Ok(())
                }
            })
            .abort(move |_, runtime, context| {
                abort_log.lock().unwrap().push("abort".into());
                let (block, reached) = (blocking.load(Ordering::SeqCst), reached.clone());
                async move {
                    if block {
                        reached.resolve();
                        aborted(runtime.signal()).await;
                    }
                    runtime.commit(|_, _| async { Ok(aborted_with("stop")) }, &context).await
                }
            }),
    );

    // Stage 1: the mark is committed while the run invocation is still active.
    let opened = open_tasks(sqlite(&path).await, vec![abortable.clone()]).await.harness;
    let id = create_in(&opened, &abortable, Value::Null).await;
    opened.resume().unwrap();
    eventually(|| async { log_of(&log).len() == 1 }).await;
    let aborting = abort_in_background(&opened, id);
    while !opened.get_task(id, &context()).await.unwrap().unwrap().abort_requested {
        flush().await;
    }
    let closer = opened.clone();
    let closing = tokio::spawn(async move { closer.close(&context()).await });
    flush().await;
    run_release.resolve();
    closing.await.unwrap().unwrap();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    assert_eq!(log_of(&log), ["run"]);

    // Stage 2: reopen dispatches the abort invocation, never the run; close while it is active.
    let opened = open_tasks(sqlite(&path).await, vec![abortable.clone()]).await.harness;
    let record = opened.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!((record.abort_requested, record.state.status()), (true, TaskStatus::Pending));
    opened.resume().unwrap();
    abort_reached.wait().await;
    opened.close(&context()).await.unwrap();
    assert_eq!(log_of(&log), ["run", "abort"]);

    // Stage 3: a fresh abort invocation settles the task.
    block.store(false, Ordering::SeqCst);
    let opened = open_tasks(sqlite(&path).await, vec![abortable.clone()]).await.harness;
    opened.resume().unwrap();
    let settled = opened.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "aborted", "reason": "stop" }));
    opened.close(&context()).await.unwrap();
    assert_eq!(log_of(&log), ["run", "abort", "abort"]);

    // Stage 4: the terminal receipt survives reopen and nothing runs again.
    let opened = open_tasks(sqlite(&path).await, vec![abortable]).await.harness;
    opened.resume().unwrap();
    flush().await;
    assert_eq!(opened.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Terminal);
    opened.close(&context()).await.unwrap();
    assert_eq!(log_of(&log), ["run", "abort", "abort"]);
}

/// A task whose run ignores its signal and whose abort handler blocks while `block_abort` is set.
fn crash_task(log: &Log, block_abort: bool) -> Task {
    let (run_log, abort_log) = (log.clone(), log.clone());
    define_task(
        TaskDefinition::new("test.crash", 1, |_| Ok(step()))
            .phase("run", move |_, _, _| {
                run_log.lock().unwrap().push("run".into());
                std::future::pending()
            })
            .abort(move |_, runtime, context| {
                abort_log.lock().unwrap().push("abort".into());
                async move {
                    if block_abort {
                        std::future::pending::<()>().await;
                    }
                    runtime.commit(|_, _| async { Ok(aborted_with("recovered")) }, &context).await
                }
            }),
    )
}

/// Crash simulation: the crashed Harness is abandoned without close, its held storage commit never lands, and its
/// blocked handlers never return. A new Harness then opens the same storage.
async fn recover(storage: &Arc<ControlledStorage>, log: &Log) -> Harness {
    storage.crash();
    open_tasks(storage.clone(), vec![crash_task(log, false)]).await.harness
}

async fn crashed_run(log: &Log) -> (Arc<ControlledStorage>, Harness, TaskId) {
    let storage = Arc::new(ControlledStorage::default());
    let crash = crash_task(log, true);
    let harness = open_tasks(storage.clone(), vec![crash.clone()]).await.harness;
    let id = create_in(&harness, &crash, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { log_of(log).len() == 1 }).await;
    (storage, harness, id)
}

fn abort_in_background(harness: &Harness, id: TaskId) -> tokio::task::JoinHandle<DurableResult<AbortTaskResult>> {
    let harness = harness.clone();
    tokio::spawn(async move { harness.abort_task(id, &context()).await })
}

#[tokio::test]
async fn crash_while_the_mark_commit_is_in_storage_the_run_resumes() {
    let log = Log::default();
    let (storage, harness, id) = crashed_run(&log).await;
    let held = storage.hold_commits();
    let _aborting = abort_in_background(&harness, id);
    held.entered().await;
    let recovered = recover(&storage, &log).await;
    let record = recovered.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!((record.abort_requested, record.state.status()), (false, TaskStatus::Pending));
    recovered.resume().unwrap();
    eventually(|| async { log_of(&log).len() == 2 }).await;
    assert_eq!(log_of(&log), ["run", "run"]);
}

#[tokio::test]
async fn crash_after_the_mark_before_the_run_joins_only_the_abort_handler_runs() {
    let log = Log::default();
    let (storage, harness, id) = crashed_run(&log).await;
    // The run ignores its signal, so the abort never finishes joining it.
    let aborting = abort_in_background(&harness, id);
    while !harness.get_task(id, &context()).await.unwrap().unwrap().abort_requested {
        flush().await;
    }
    flush().await;
    assert!(!aborting.is_finished());
    let recovered = recover(&storage, &log).await;
    let record = recovered.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!((record.abort_requested, record.state.status()), (true, TaskStatus::Pending));
    recovered.resume().unwrap();
    let settled = recovered.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "aborted", "reason": "recovered" }));
    assert_eq!(log_of(&log), ["run", "abort"]);
    recovered.close(&context()).await.unwrap();
}

#[tokio::test]
async fn crash_while_the_abort_handler_runs_a_fresh_abort_invocation_settles_the_task() {
    let log = Log::default();
    let storage = Arc::new(ControlledStorage::default());
    let crash = crash_task(&log, true);
    let harness = open_tasks(storage.clone(), vec![crash.clone()]).await.harness;
    let id = create_in(&harness, &crash, Value::Null).await;
    harness.abort_task(id, &context()).await.unwrap();
    harness.resume().unwrap();
    eventually(|| async { log_of(&log).len() == 1 }).await;
    assert_eq!(log_of(&log), ["abort"]);
    assert_eq!(harness.get_task(id, &context()).await.unwrap().unwrap().state.status(), TaskStatus::Running);
    let recovered = recover(&storage, &log).await;
    recovered.resume().unwrap();
    let settled = recovered.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "aborted", "reason": "recovered" }));
    assert_eq!(log_of(&log), ["abort", "abort"]);
    recovered.close(&context()).await.unwrap();
}

#[tokio::test]
async fn crash_while_the_abort_outcome_is_in_storage_the_abort_handler_runs_again() {
    let log = Log::default();
    let storage = Arc::new(ControlledStorage::default());
    let (reached, proceed) = (Deferred::default(), Deferred::default());
    let (abort_log, signal, gate) = (log.clone(), reached.clone(), proceed.clone());
    let crash = define_task(
        TaskDefinition::new("test.crash", 1, |_| Ok(step())).phase("run", |_, _, _| async { Ok(()) }).abort(
            move |_, runtime, context| {
                abort_log.lock().unwrap().push("abort".into());
                let (reached, proceed) = (signal.clone(), gate.clone());
                async move {
                    reached.resolve();
                    proceed.wait().await;
                    runtime.commit(|_, _| async { Ok(aborted_with("lost")) }, &context).await
                }
            },
        ),
    );
    let harness = open_tasks(storage.clone(), vec![crash.clone()]).await.harness;
    let id = create_in(&harness, &crash, Value::Null).await;
    harness.abort_task(id, &context()).await.unwrap();
    harness.resume().unwrap();
    reached.wait().await;
    let held = storage.hold_commits();
    proceed.resolve();
    held.entered().await;
    let recovered = recover(&storage, &log).await;
    recovered.resume().unwrap();
    let settled = recovered.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "aborted", "reason": "recovered" }));
    assert_eq!(log_of(&log), ["abort", "abort"]);
    recovered.close(&context()).await.unwrap();
}

#[tokio::test]
async fn crash_after_the_terminal_outcome_nothing_runs_again() {
    let log = Log::default();
    let storage = Arc::new(ControlledStorage::default());
    let crash = crash_task(&log, false);
    let harness = open_tasks(storage.clone(), vec![crash.clone()]).await.harness;
    let id = create_in(&harness, &crash, Value::Null).await;
    harness.abort_task(id, &context()).await.unwrap();
    harness.resume().unwrap();
    harness.wait_for_task(id, &context()).await.unwrap();
    let recovered = recover(&storage, &log).await;
    recovered.resume().unwrap();
    flush().await;
    assert_eq!(log_of(&log), ["abort"]);
    let state = serde_json::to_value(recovered.get_task(id, &context()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state, json!({ "status": "terminal", "outcome": { "status": "aborted", "reason": "recovered" } }));
    recovered.close(&context()).await.unwrap();
}

#[tokio::test]
async fn crash_while_the_reservation_commit_is_in_storage_the_task_is_still_pending() {
    let log = Log::default();
    let storage = Arc::new(ControlledStorage::default());
    let crash = crash_task(&log, false);
    let harness = open_tasks(storage.clone(), vec![crash.clone()]).await.harness;
    let id = create_in(&harness, &crash, Value::Null).await;
    let held = storage.hold_commits();
    harness.resume().unwrap();
    held.entered().await;
    let recovered = recover(&storage, &log).await;
    assert_eq!(recovered.get_task(id, &context()).await.unwrap().unwrap().state.status(), TaskStatus::Pending);
    assert!(log_of(&log).is_empty());
    recovered.abort_task(id, &context()).await.unwrap();
    recovered.resume().unwrap();
    recovered.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(log_of(&log), ["abort"]);
    recovered.close(&context()).await.unwrap();
}
