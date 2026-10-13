//! Port of the upstream test file for `Harness` inspection. concise-ignore

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::{DurableError, EntryDraft, TaskId, TaskState};
use serde_json::json;
use support::harness_support::*;
use support::{Deferred, context};

fn completed() -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: pi_durable::TaskOutcome::Completed { result: Some(Json::Null) } })
}

/// A one-phase task that waits for `gate`, when given, and completes.
fn task(name: &str, version: u32, gate: Option<Deferred>) -> TaskDefinition {
    TaskDefinition::new(name, version, |_| Ok(Json::from(json!({ "phase": "run" }))))
        .phase("run", move |_, runtime, context| {
            let gate = gate.clone();
            async move {
                if let Some(gate) = gate {
                    gate.wait().await;
                }
                runtime.commit(|_, _| async { Ok(completed()) }, &context).await
            }
        })
        .abort(|_, _, _| async { Ok(()) })
}

fn state_of(inspection: &HarnessInspection, id: TaskId) -> Option<TaskInspectionState> {
    inspection.tasks.iter().find(|entry| entry.record.id == id).map(|entry| entry.state.clone())
}

#[tokio::test]
async fn derives_every_live_task_state_without_running_task_code() {
    let gate = Deferred::default();
    let gate_task = define_task(task("test.gate", 1, Some(gate.clone())));
    let migrations = Arc::new(AtomicUsize::new(0));
    let counter = migrations.clone();
    let migrating = define_task(task("test.migrating", 2, None).migrate(move |input, checkpoint, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok((input.clone(), checkpoint.clone()))
    }));
    let failing =
        define_task(task("test.failing", 2, None).migrate(|_, _, _| Err(DurableError::error("cannot migrate"))));
    let registered = [
        gate_task.clone(),
        migrating,
        define_task(task("test.no-migration", 2, None)),
        failing,
        define_task(task("test.too-old", 1, None)),
    ];
    let registry = Arc::new(create_registry());
    registry.install(registered.iter().cloned().fold(Extension::new("tasks"), Extension::task)).unwrap();
    let options = HarnessOptions::new(chat_setup(Default::default()).models, registry as Arc<dyn RegistryReader>);
    let harness = Harness::open(Arc::new(MemoryStorage::new()), options, &context()).await.unwrap();
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create = move |tx: Tx| async move {
        let mut ids = Vec::new();
        // Stored by definitions other than the registered ones.
        let stored = [
            gate_task,
            define_task(task("test.migrating", 1, None)),
            define_task(task("test.no-migration", 1, None)),
            define_task(task("test.failing", 1, None)),
            define_task(task("test.too-old", 2, None)),
            define_task(task("test.missing", 1, None)),
        ];
        for template in stored {
            ids.push(tx.create_task(template, Json::Null, TaskOptions::conversation(id)).await?);
        }
        Ok(ids)
    };
    let ids = root.commit(create, &context()).await.unwrap();
    let [gate_id, migrating_id, no_migration, failing_id, too_old, missing] = ids[..] else { unreachable!() };

    let paused = harness.inspect(&context()).await.unwrap();
    assert_eq!(paused.scheduling, Scheduling::Paused);
    assert_eq!(paused.tasks.iter().map(|entry| entry.record.id).collect::<Vec<_>>(), ids);
    assert_eq!(state_of(&paused, gate_id), Some(TaskInspectionState::Ready { migrates: false }));
    assert_eq!(state_of(&paused, migrating_id), Some(TaskInspectionState::Ready { migrates: true }));
    // A migration that was never tried is not run to find out.
    assert_eq!(state_of(&paused, failing_id), Some(TaskInspectionState::Ready { migrates: true }));
    let Some(TaskInspectionState::Blocked { reason, error }) = state_of(&paused, no_migration) else {
        panic!("expected a blocked task");
    };
    assert_eq!(reason, BlockedReason::MigrationFailed);
    assert_eq!(error.as_deref(), Some("Task test.no-migration version 2 has no migration from 1"));
    let blocked = |reason| Some(TaskInspectionState::Blocked { reason, error: None });
    assert_eq!(state_of(&paused, too_old), blocked(BlockedReason::TaskTooOld));
    assert_eq!(state_of(&paused, missing), blocked(BlockedReason::MissingTask));
    assert_eq!(migrations.load(Ordering::SeqCst), 0);

    harness.resume().unwrap();
    harness.wait_for_task(migrating_id, &context()).await.unwrap();
    assert_eq!(migrations.load(Ordering::SeqCst), 1);
    wait_for(|| {
        let harness = harness.clone();
        async move {
            let running = harness.inspect(&context()).await.unwrap();
            state_of(&running, gate_id) == Some(TaskInspectionState::Running)
                && matches!(state_of(&running, failing_id), Some(TaskInspectionState::Blocked { .. }))
        }
    })
    .await;
    let running = harness.inspect(&context()).await.unwrap();
    assert_eq!(running.scheduling, Scheduling::Running);
    assert_eq!(state_of(&running, migrating_id), None);
    let Some(TaskInspectionState::Blocked { reason, error }) = state_of(&running, failing_id) else {
        panic!("expected a blocked task");
    };
    assert_eq!((reason, error.as_deref()), (BlockedReason::MigrationFailed, Some("cannot migrate")));

    gate.resolve();
    harness.wait_for_task(gate_id, &context()).await.unwrap();
    let settled = harness.inspect(&context()).await.unwrap();
    let left: Vec<_> = settled.tasks.iter().map(|entry| entry.record.id).collect();
    assert_eq!(left, [no_migration, failing_id, too_old, missing]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lists_unsettled_submissions() {
    let setup = chat_setup(Default::default());
    let (step, reached) = unanswered();
    setup.faux.set_responses([step]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let other = harness.create_conversation(CreateOptions::default(), &context()).await.unwrap();
    other.submit(SubmissionDraft::write(EntryDraft::new("note")), &context()).await.unwrap();
    let input = root.submit(SubmissionDraft::input("hi"), &context()).await.unwrap();
    reached.notified().await;

    let inspection = harness.inspect(&context()).await.unwrap();
    assert_eq!(inspection.submissions, [input.status(&context()).await.unwrap()]);
    let tasks: Vec<_> = inspection.tasks.iter().map(|entry| (entry.record.kind.clone(), entry.state.clone())).collect();
    assert_eq!(tasks, [("pi.generation".to_owned(), TaskInspectionState::Running)]);
    harness.close(&context()).await.unwrap();
}
