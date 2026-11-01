//! Definition handover and Harness open.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::registry::{RegistryListener, RegistryUnsubscribe};
use pi_durable::harness::*;
use pi_durable::models::create_models;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::*;
use serde_json::{Value, json};

use super::blocked::add_task;
use super::support::task_support::{aborted_with, completed, eventually, open_tasks, open_tasks_with};
use super::support::{ControlledStorage, Deferred, context, flush};
use super::{Log, create_in, log_of, outcome};

#[derive(Clone, Default)]
struct Gates {
    a: Option<Deferred>,
    b: Option<Deferred>,
    on_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn checkpoint(phase: &str) -> Json {
    Json::from(json!({ "phase": phase }))
}

/// The "test.handover" task: phases a and b advance after their gate, and c completes. With `migrate_to`, older
/// records migrate to that phase.
fn handover_task(
    label: &'static str,
    version: u32,
    log: &Log,
    gates: Gates,
    migrate_to: Option<Result<&'static str, &'static str>>,
) -> Task {
    let advance = |phase: &'static str, next: &'static str| {
        let (log, gates) = (log.clone(), gates.clone());
        move |_: TaskRecord, runtime: TaskRuntime, context: pi_chord::Context| {
            let (log, gates) = (log.clone(), gates.clone());
            async move {
                log.lock().unwrap().push(format!("{label}:{phase} start"));
                if let Some(gate) = if phase == "a" { &gates.a } else { &gates.b } {
                    gate.wait().await;
                }
                let state = Some(TaskState::Running { checkpoint: checkpoint(next) });
                runtime.commit(move |_, _| async move { Ok(state) }, &context).await?;
                // Leave room for a wrongly dispatched successor before this invocation ends.
                flush().await;
                if let Some(on_end) = &gates.on_end {
                    on_end();
                }
                log.lock().unwrap().push(format!("{label}:{phase} end"));
                Ok(())
            }
        }
    };
    let (c_log, abort_log) = (log.clone(), log.clone());
    let definition = TaskDefinition::new("test.handover", version, |_| Ok(checkpoint("a")))
        .phase("a", advance("a", "b"))
        .phase("b", advance("b", "c"))
        .phase("c", move |_, runtime, context| {
            c_log.lock().unwrap().push(format!("{label}:c"));
            async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context).await }
        })
        .abort(move |_, runtime, context| {
            abort_log.lock().unwrap().push(format!("{label}:abort"));
            async move { runtime.commit(move |_, _| async move { Ok(aborted_with(label)) }, &context).await }
        });
    define_task(match migrate_to {
        Some(Ok(phase)) => definition.migrate(move |_, _, _| Ok((Json::Null, checkpoint(phase)))),
        Some(Err(message)) => definition.migrate(move |_, _, _| Err(DurableError::error(message))),
        None => definition,
    })
}

struct Started {
    harness: Harness,
    registry: Arc<Registry>,
    reports: Arc<Mutex<Vec<DurableError>>>,
    infos: Arc<Mutex<Vec<ReportInfo>>>,
    id: TaskId,
}

fn handover_report(id: TaskId) -> ReportTask {
    ReportTask { id, kind: "test.handover".into(), conversation_id: ROOT_CONVERSATION_ID }
}

async fn start_handover(log: &Log, gates: Gates, storage: Arc<dyn Storage>) -> Started {
    start_handover_at(log, gates, storage, 1).await
}

/// `start_handover()` with the running definition at `version`.
async fn start_handover_at(log: &Log, gates: Gates, storage: Arc<dyn Storage>, version: u32) -> Started {
    let registry = Arc::new(create_registry());
    add_task(&registry, handover_task("old", version, log, gates, None));
    let opened = open_tasks_with(storage, vec![], registry.clone(), |options| options).await;
    let task = handover_task("old", version, log, Gates::default(), None);
    let id = create_in(&opened.harness, &task, Value::Null).await;
    opened.harness.resume().unwrap();
    eventually(|| async { log_of(log).len() == 1 }).await;
    Started { harness: opened.harness, registry, reports: opened.reports, infos: opened.infos, id }
}

fn gate_a(gate: &Deferred) -> Gates {
    Gates { a: Some(gate.clone()), ..Gates::default() }
}

#[tokio::test]
async fn hands_over_at_the_next_phase_boundary_to_a_same_version_replacement_without_overlap() {
    let (log, gate) = (Log::default(), Deferred::default());
    let Started { harness, registry, id, .. } =
        start_handover(&log, gate_a(&gate), Arc::new(MemoryStorage::new())).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, handover_task("new", 1, &log, Gates::default(), None));
    gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(log_of(&log), ["old:a start", "old:a end", "new:b start", "new:b end", "new:c"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_the_memos_across_a_handover() {
    let log = Log::default();
    let (gate, reached) = (Deferred::default(), Deferred::default());
    let memo_task = |label: &'static str| {
        let (gate, reached, log) = (gate.clone(), reached.clone(), log.clone());
        define_task(
            TaskDefinition::new("test.handover-memo", 1, |_| Ok(checkpoint("a")))
                .phase("a", move |_, runtime, context| {
                    let (gate, reached) = (gate.clone(), reached.clone());
                    async move {
                        runtime.memo_or("picked", Json::from(label), &context).await?;
                        reached.resolve();
                        gate.wait().await;
                        let state = Some(TaskState::Running { checkpoint: checkpoint("b") });
                        runtime.commit(move |_, _| async move { Ok(state) }, &context).await
                    }
                })
                .phase("b", move |_, runtime, context| {
                    let log = log.clone();
                    async move {
                        // The candidate loses to the memo the old definition stored.
                        let picked = runtime.memo_or("picked", Json::from(label), &context).await?;
                        log.lock().unwrap().push(format!("{label}:b {}", picked.as_str().unwrap()));
                        let result = picked.to_serde();
                        runtime.commit(move |_, _| async move { Ok(completed(result)) }, &context).await
                    }
                })
                .abort(|_, _, _| async { Ok(()) }),
        )
    };
    let registry = Arc::new(create_registry());
    add_task(&registry, memo_task("old"));
    let harness =
        open_tasks_with(Arc::new(MemoryStorage::new()), vec![], registry.clone(), |options| options).await.harness;
    let id = create_in(&harness, &memo_task("old"), Value::Null).await;
    harness.resume().unwrap();
    reached.wait().await;
    add_task(&registry, memo_task("new"));
    gate.resolve();
    let receipt = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&receipt), json!({ "status": "completed", "result": "old" }));
    assert_eq!(log_of(&log), ["new:b old"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn hands_over_to_a_newer_version_with_a_migration() {
    let (log, gate) = (Log::default(), Deferred::default());
    let Started { harness, registry, id, .. } =
        start_handover(&log, gate_a(&gate), Arc::new(MemoryStorage::new())).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, handover_task("v2", 2, &log, Gates::default(), Some(Ok("c"))));
    gate.resolve();
    let receipt = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(receipt.version, 2);
    assert_eq!(log_of(&log), ["old:a start", "old:a end", "v2:c"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn hands_over_to_a_newer_version_whose_migration_fails_and_leaves_the_task_blocked() {
    let (log, gate) = (Log::default(), Deferred::default());
    let Started { harness, registry, id, infos, .. } =
        start_handover(&log, gate_a(&gate), Arc::new(MemoryStorage::new())).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, handover_task("broken", 2, &log, Gates::default(), Some(Err("broken migration"))));
    gate.resolve();
    eventually(|| async { infos.lock().unwrap().len() == 1 }).await;
    let extension = Some("task:test.handover".to_owned());
    assert_eq!(*infos.lock().unwrap(), [ReportInfo::MigrationFailed { task: handover_report(id), extension }]);
    flush().await;
    let record = harness.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!(record.version, 1);
    assert_eq!(
        serde_json::to_value(&record.state).unwrap(),
        json!({ "status": "pending", "checkpoint": { "phase": "b" } })
    );
    assert_eq!(log_of(&log), ["old:a start", "old:a end"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_running_under_the_old_definition_when_the_replacement_is_missing_or_cannot_take_the_task() {
    let log = Log::default();
    let (gate_a, gate_b) = (Deferred::default(), Deferred::default());
    let gates = Gates { a: Some(gate_a.clone()), b: Some(gate_b.clone()), on_end: None };
    let Started { harness, registry, id, reports, infos } =
        start_handover(&log, gates, Arc::new(MemoryStorage::new())).await;
    registry.uninstall("task:test.handover").unwrap();
    gate_a.resolve();
    eventually(|| async { log_of(&log).contains(&"old:b start".to_owned()) }).await;
    // A newer definition without a migration cannot take the task either.
    add_task(&registry, handover_task("unmigratable", 2, &log, Gates::default(), None));
    gate_b.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(log_of(&log), ["old:a start", "old:a end", "old:b start", "old:b end", "old:c"]);
    let reports: Vec<String> = reports.lock().unwrap().iter().map(ToString::to_string).collect();
    let message = format!("Task {id} keeps running under its old test.handover definition");
    assert_eq!(reports, [message.clone(), message]);
    let outdated = |reason| ReportInfo::TaskOutdated {
        task: handover_report(id),
        extension: Some("task:test.handover".into()),
        reason,
    };
    let expected = [outdated(OutdatedReason::MissingTask), outdated(OutdatedReason::MigrationFailed)];
    assert_eq!(*infos.lock().unwrap(), expected);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reports_a_replacement_older_than_the_running_task_as_task_too_old_and_keeps_the_old_definition() {
    let (log, gate) = (Log::default(), Deferred::default());
    let Started { harness, registry, id, infos, .. } =
        start_handover_at(&log, gate_a(&gate), Arc::new(MemoryStorage::new()), 2).await;
    // The same extension name replaces the running definition with an older version.
    add_task(&registry, handover_task("older", 1, &log, Gates::default(), None));
    gate.resolve();
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(log_of(&log), ["old:a start", "old:a end", "old:b start", "old:b end", "old:c"]);
    let extension = Some("task:test.handover".to_owned());
    let outdated =
        ReportInfo::TaskOutdated { task: handover_report(id), extension, reason: OutdatedReason::TaskTooOld };
    assert_eq!(*infos.lock().unwrap(), [outdated]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_runtime_commit_of_the_old_invocation_queued_behind_its_handover_commit() {
    let (log, gate) = (Log::default(), Deferred::default());
    let storage = Arc::new(ControlledStorage::default());
    let held = Arc::new(Mutex::new(None));
    let old_runtime = Arc::new(Mutex::new(None::<TaskRuntime>));
    let harness_ref = Arc::new(Mutex::new(None::<Harness>));
    let (phase_gate, phase_storage, phase_held) = (gate.clone(), storage.clone(), held.clone());
    let (phase_runtime, phase_harness) = (old_runtime.clone(), harness_ref.clone());
    let old = define_task(
        TaskDefinition::new("test.handover", 1, |_| Ok(checkpoint("a")))
            .phase("a", move |task, runtime, context| {
                *phase_runtime.lock().unwrap() = Some(runtime.clone());
                let (gate, storage, held) = (phase_gate.clone(), phase_storage.clone(), phase_held.clone());
                let harness = phase_harness.lock().unwrap().clone().unwrap();
                async move {
                    gate.wait().await;
                    let state = Some(TaskState::Running { checkpoint: checkpoint("b") });
                    runtime.commit(move |_, _| async move { Ok(state) }, &context).await?;
                    // Hold the line with an unrelated commit so the handover commit queues behind it.
                    *held.lock().unwrap() = Some(storage.hold_commits());
                    let conversation = task.conversation_id;
                    let blocker =
                        move |tx: Tx| async move { tx.append_entry(conversation, EntryDraft::new("blocker")).await };
                    tokio::spawn(harness.commit(blocker, &context));
                    Ok(())
                }
            })
            .phase("b", |_, _, _| async { Ok(()) })
            .phase("c", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let registry = Arc::new(create_registry());
    add_task(&registry, old.clone());
    let harness = open_tasks_with(storage.clone(), vec![], registry.clone(), |options| options).await.harness;
    *harness_ref.lock().unwrap() = Some(harness.clone());
    let id = create_in(&harness, &old, Value::Null).await;
    harness.resume().unwrap();
    eventually(|| async { old_runtime.lock().unwrap().is_some() }).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, handover_task("new", 1, &log, Gates::default(), None));
    gate.resolve();
    eventually(|| async { held.lock().unwrap().is_some() }).await;
    let held = held.lock().unwrap().clone().unwrap();
    held.entered().await;
    flush().await;
    // Queued behind the handover commit while the invocation has not ended yet.
    let runtime = old_runtime.lock().unwrap().clone().unwrap();
    let late =
        tokio::spawn(async move { runtime.commit(|_, _| async { Ok(completed(Value::Null)) }, &context()).await });
    flush().await;
    held.release();
    let error = late.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("invocation has ended"), "{error}");
    harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(log_of(&log), ["new:b start", "new:b end", "new:c"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn preserves_an_abort_mark_that_races_the_handover_commit_the_new_definition_aborts() {
    let (log, gate) = (Log::default(), Deferred::default());
    let storage = Arc::new(ControlledStorage::default());
    let held = Arc::new(Mutex::new(None));
    // The progress commit landed; hold the next commit, the handover, and queue the abort mark behind it.
    let (hold_storage, hold) = (storage.clone(), held.clone());
    let on_end: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let mut held = hold.lock().unwrap();
        if held.is_none() {
            *held = Some(hold_storage.hold_commits());
        }
    });
    let gates = Gates { a: Some(gate.clone()), b: None, on_end: Some(on_end) };
    let Started { harness, registry, id, .. } = start_handover(&log, gates, storage.clone()).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, handover_task("new", 1, &log, Gates::default(), None));
    gate.resolve();
    eventually(|| async { held.lock().unwrap().is_some() }).await;
    let held = held.lock().unwrap().clone().unwrap();
    held.entered().await;
    let aborter = harness.clone();
    let aborting = tokio::spawn(async move { aborter.abort_task(id, &context()).await });
    flush().await;
    held.release();
    assert_eq!(aborting.await.unwrap().unwrap(), AbortTaskResult::Marked);
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "aborted", "reason": "new" }));
    let states: Vec<String> = storage
        .commits()
        .iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::Task { value } if value.id == id => {
                let mark = if value.abort_requested { "+mark" } else { "" };
                Some(format!("{}{mark}", value.state.status().as_str()))
            }
            _ => None,
        })
        .collect();
    // created, reserved, progress, handover, mark, abort reservation, aborted
    let expected = ["pending", "running", "running", "pending", "pending+mark", "running+mark", "terminal+mark"];
    assert_eq!(states, expected);
    assert_eq!(log_of(&log), ["old:a start", "old:a end", "new:abort"]);
    harness.close(&context()).await.unwrap();
}

/// A registry reader that counts live subscriptions.
struct CountingReader {
    registry: Registry,
    count: Arc<AtomicUsize>,
}

impl RegistryReader for CountingReader {
    fn snapshot(&self) -> RegistrySnapshot {
        self.registry.snapshot()
    }

    fn subscribe(&self, listener: RegistryListener) -> RegistryUnsubscribe {
        self.count.fetch_add(1, Ordering::SeqCst);
        let unsubscribe = self.registry.subscribe(listener);
        let count = self.count.clone();
        Box::new(move || {
            count.fetch_sub(1, Ordering::SeqCst);
            unsubscribe();
        })
    }
}

#[tokio::test]
async fn releases_its_registry_subscription_and_closes_the_session_when_open_fails() {
    let storage = Arc::new(ControlledStorage::default());
    let log = Log::default();
    let run_log = log.clone();
    let stuck = define_task(
        TaskDefinition::new("test.stuck", 1, |_| Ok(checkpoint("run")))
            .phase("run", move |_, _, _| {
                run_log.lock().unwrap().push("run".into());
                std::future::pending()
            })
            .abort(|_, _, _| async { Ok(()) }),
    );
    // Leave a running task behind so open has a reconciliation commit to fail.
    let first = open_tasks(storage.clone(), vec![stuck.clone()]).await.harness;
    create_in(&first, &stuck, Value::Null).await;
    first.resume().unwrap();
    eventually(|| async { log_of(&log).len() == 1 }).await;

    let count = Arc::new(AtomicUsize::new(0));
    let reader = Arc::new(CountingReader { registry: create_registry(), count: count.clone() });
    storage.fail_next_commit(DurableError::error("disk full"));
    let options = HarnessOptions::new(Arc::new(create_models()), reader);
    let error = Harness::open(storage.clone(), options, &context()).await.err().unwrap();
    assert!(error.to_string().contains("disk full"), "{error}");
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(storage.task(TaskId::from(1), &context()).await.is_err());
}
