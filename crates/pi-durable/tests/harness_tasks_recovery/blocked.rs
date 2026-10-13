//! Blocked tasks: missing definitions, newer versions, and migrations.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use pi_chord::Json;
use pi_durable::documents::{DocSpec, DocToken, TaskScope, define_doc};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::support::Deferred;
use super::support::chat::text_tool;
use super::support::task_support::{aborted, aborted_with, completed, eventually, open_tasks, open_tasks_with};
use super::support::{context, flush};
use super::{create_in, outcome, sqlite, sqlite_path, step};

type Migrate = Box<dyn Fn(&Json, &Json, u32) -> DurableResult<(Json, Json)> + Send + Sync>;

/// A versioned one-phase task that completes with `result`, optionally migrating older records.
pub fn versioned(version: u32, result: &'static str, migrate: Option<Migrate>) -> Task {
    let definition = TaskDefinition::new("test.versioned", version, |_| Ok(step()))
        .phase("run", move |_, runtime, context| async move {
            runtime.commit(move |_, _| async move { Ok(completed(json!(result))) }, &context).await
        })
        .abort(move |_, runtime, context| async move {
            runtime.commit(move |_, _| async move { Ok(aborted_with(result)) }, &context).await
        });
    define_task(match migrate {
        Some(migrate) => definition.migrate(migrate),
        None => definition,
    })
}

/// Install `task` as upstream `addTask` does, under the extension name `task:<name>`.
pub fn add_task(registry: &Registry, task: Task) {
    registry.install(Extension::new(format!("task:{}", task.name)).task(task)).unwrap();
}

fn reports_of(reports: &Mutex<Vec<DurableError>>) -> Vec<String> {
    reports.lock().unwrap().iter().map(ToString::to_string).collect()
}

#[tokio::test]
async fn keeps_a_task_with_a_missing_definition_pending_and_live_for_idle_waits_until_registration() {
    let opened = open_tasks(Arc::new(MemoryStorage::new()), vec![]).await;
    let harness = opened.harness;
    let id = create_in(&harness, &versioned(1, "v1", None), Value::Null).await;
    harness.resume().unwrap();
    flush().await;
    assert_eq!(harness.get_task(id, &context()).await.unwrap().unwrap().state.status(), TaskStatus::Pending);
    let waiter = harness.clone();
    let idle = tokio::spawn(async move { waiter.wait_for_idle(&context()).await });
    flush().await;
    assert!(!idle.is_finished());
    add_task(&opened.registry, versioned(1, "v1", None));
    idle.await.unwrap().unwrap();
    let state = serde_json::to_value(harness.get_task(id, &context()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state, json!({ "status": "terminal", "outcome": { "status": "completed", "result": "v1" } }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_task_stored_by_a_newer_version_pending_until_a_fitting_definition_is_registered() {
    let registry = Arc::new(create_registry());
    add_task(&registry, versioned(1, "old", None));
    let harness =
        open_tasks_with(Arc::new(MemoryStorage::new()), vec![], registry.clone(), |options| options).await.harness;
    let id = create_in(&harness, &versioned(2, "new", None), Value::Null).await;
    harness.resume().unwrap();
    flush().await;
    assert_eq!(harness.get_task(id, &context()).await.unwrap().unwrap().state.status(), TaskStatus::Pending);
    // The same extension name replaces the old one in place.
    add_task(&registry, versioned(2, "new", None));
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "completed", "result": "new" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn migrates_at_reservation_leaves_the_record_unchanged_when_migration_fails_and_retries_only_for_a_new_definition()
 {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let opened = open_tasks(sqlite(&path).await, vec![]).await;
    let id = create_in(&opened.harness, &versioned(1, "v1", None), Value::Null).await;
    opened.harness.close(&context()).await.unwrap();

    let registry = Arc::new(create_registry());
    let failures = Arc::new(AtomicUsize::new(0));
    let count = failures.clone();
    let failing: Migrate = Box::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(DurableError::error("cannot migrate"))
    });
    add_task(&registry, versioned(2, "v2", Some(failing)));
    let opened = open_tasks_with(sqlite(&path).await, vec![], registry.clone(), |options| options).await;
    opened.harness.resume().unwrap();
    eventually(|| async { opened.reports.lock().unwrap().len() == 1 }).await;
    // An unrelated registry change wakes the scheduler without retrying the same failed definition.
    let unrelated = text_tool("unrelated");
    add_tool_to(&registry, unrelated);
    flush().await;
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    let reports = reports_of(&opened.reports);
    assert_eq!(reports.len(), 1);
    assert!(reports[0].contains("cannot migrate"), "{reports:?}");
    let record = opened.harness.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!(record.version, 1);
    assert_eq!(
        serde_json::to_value(&record.state).unwrap(),
        json!({ "status": "pending", "checkpoint": { "phase": "run" } })
    );

    let migrations = Arc::new(Mutex::new(Vec::new()));
    let sink = migrations.clone();
    let migrate: Migrate = Box::new(move |_, checkpoint, from| {
        sink.lock().unwrap().push(from);
        Ok((Json::Null, checkpoint.clone()))
    });
    // The same extension name replaces the old one in place.
    add_task(&registry, versioned(2, "v2", Some(migrate)));
    let receipt = opened.harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!((receipt.version, outcome(&receipt)), (2, json!({ "status": "completed", "result": "v2" })));
    assert_eq!(*migrations.lock().unwrap(), [1]);
    opened.harness.close(&context()).await.unwrap();
}

fn add_tool_to(registry: &Registry, tool: ToolRegistration) {
    registry.install(Extension::new(format!("tool:{}", tool.name())).tool(tool)).unwrap();
}

#[tokio::test]
async fn blocks_an_older_record_whose_newer_definition_has_no_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let opened = open_tasks(sqlite(&path).await, vec![]).await;
    let id = create_in(&opened.harness, &versioned(1, "v1", None), Value::Null).await;
    opened.harness.close(&context()).await.unwrap();
    let opened = open_tasks(sqlite(&path).await, vec![versioned(2, "v2", None)]).await;
    opened.harness.resume().unwrap();
    eventually(|| async { opened.reports.lock().unwrap().len() == 1 }).await;
    let reports = reports_of(&opened.reports);
    assert!(reports[0].contains("has no migration from 1"), "{reports:?}");
    let record = opened.harness.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!((record.version, record.state.status()), (1, TaskStatus::Pending));
    opened.harness.close(&context()).await.unwrap();
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Scratch {
    n: u32,
}

static SCRATCH: LazyLock<DocToken<Scratch, TaskScope>> =
    LazyLock::new(|| define_doc(DocSpec::new("test.orphan-scratch", 1, TaskScope, Scratch::default)).unwrap());

#[tokio::test]
async fn settles_an_aborted_blocked_task_as_orphaned_and_retires_its_documents() {
    let harness = open_tasks(Arc::new(MemoryStorage::new()), vec![]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let conversation = root.id;
    let create = move |tx: Tx| async move {
        let options = TaskOptions::conversation(conversation);
        let created = tx.create_task(versioned(1, "x", None), Json::Null, options).await?;
        tx.doc(&SCRATCH, created).await?.edit(|scratch| scratch.n = 1)?;
        Ok(created)
    };
    let id = root.commit(create, &context()).await.unwrap();
    // Before resume: the marking commit settles the blocked task directly.
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    let state = serde_json::to_value(harness.get_task(id, &context()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state, json!({ "status": "terminal", "outcome": { "status": "orphaned", "reason": "missing_task" } }));
    assert!(harness.snapshot(&SCRATCH, id, &context()).await.unwrap().is_none());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn orphans_a_marked_task_whose_definition_disappeared_while_its_run_was_active() {
    let reached = Deferred::default();
    let signal = reached.clone();
    let running = define_task(
        TaskDefinition::new("test.vanishing", 1, |_| Ok(step()))
            .phase("run", move |_, runtime, _| {
                let reached = signal.clone();
                async move {
                    reached.resolve();
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, _, _| async { panic!("must not run") }),
    );
    let registry = Arc::new(create_registry());
    add_task(&registry, running.clone());
    let harness =
        open_tasks_with(Arc::new(MemoryStorage::new()), vec![], registry.clone(), |options| options).await.harness;
    let id = create_in(&harness, &running, Value::Null).await;
    harness.resume().unwrap();
    reached.wait().await;
    registry.uninstall("task:test.vanishing").unwrap();
    // An active run means the mark does not orphan directly; the scheduler orphans once the run has ended.
    assert_eq!(harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    let settled = harness.wait_for_task(id, &context()).await.unwrap();
    assert_eq!(outcome(&settled), json!({ "status": "orphaned", "reason": "missing_task" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn orphans_a_reopened_abort_marked_task_without_a_definition_once_scheduling_resumes() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let opened = open_tasks(sqlite(&path).await, vec![versioned(1, "x", None)]).await;
    let id = create_in(&opened.harness, &versioned(1, "x", None), Value::Null).await;
    assert_eq!(opened.harness.abort_task(id, &context()).await.unwrap(), AbortTaskResult::Marked);
    opened.harness.close(&context()).await.unwrap();

    let harness = open_tasks(sqlite(&path).await, vec![]).await.harness;
    let record = harness.get_task(id, &context()).await.unwrap().unwrap();
    assert_eq!((record.abort_requested, record.state.status()), (true, TaskStatus::Pending));
    let waiter = harness.clone();
    let idle = tokio::spawn(async move { waiter.wait_for_idle(&context()).await });
    flush().await;
    harness.resume().unwrap();
    idle.await.unwrap().unwrap();
    let state = serde_json::to_value(harness.get_task(id, &context()).await.unwrap().unwrap().state).unwrap();
    assert_eq!(state, json!({ "status": "terminal", "outcome": { "status": "orphaned", "reason": "missing_task" } }));
    harness.close(&context()).await.unwrap();
}
