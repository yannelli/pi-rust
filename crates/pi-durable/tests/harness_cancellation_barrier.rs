//! Port of upstream `test/harness-cancellation-barrier.test.ts` (work below a cancelled owner).

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;
use support::task_support::{aborted, eventually, open_tasks};
use support::{ControlledStorage, Deferred, context, flush};

fn aborted_outcome() -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } })
}

/// Runs until aborted; its abort handler ends it `aborted`.
fn dormant(name: &str) -> Task {
    define_task(
        TaskDefinition::new(name, 1, |_| Ok(Json::from(json!({ "phase": "work" }))))
            .phase("work", |_, runtime, _| async move {
                aborted(runtime.signal()).await;
                Ok(())
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_outcome()) }, &context).await
            }),
    )
}

async fn outcome_status(harness: &Harness, id: TaskId) -> &'static str {
    harness.wait_for_task(id, &context()).await.unwrap().state.outcome().unwrap().status()
}

#[tokio::test]
async fn does_not_start_its_next_phase_while_the_cascade_that_marks_it_has_not_committed() {
    let storage = Arc::new(ControlledStorage::default());
    let (entered, advance) = (Deferred::default(), Deferred::default());
    let ran_next = Arc::new(AtomicBool::new(false));
    let (first_entered, first_advance, next_ran) = (entered.clone(), advance.clone(), ran_next.clone());
    let child_task = define_task(
        TaskDefinition::new("test.child", 1, |_| Ok(Json::from(json!({ "phase": "first" }))))
            .phase("first", move |_, runtime, context| {
                let (entered, advance) = (first_entered.clone(), first_advance.clone());
                async move {
                    entered.resolve();
                    advance.wait().await;
                    let next = Some(TaskState::Running { checkpoint: Json::from(json!({ "phase": "next" })) });
                    runtime.commit(move |_, _| async move { Ok(next) }, &context).await
                }
            })
            .phase("next", move |_, runtime, _| {
                next_ran.store(true, Ordering::SeqCst);
                async move {
                    aborted(runtime.signal()).await;
                    Ok(())
                }
            })
            .abort(|_, runtime, context| async move {
                runtime.commit(|_, _| async { Ok(aborted_outcome()) }, &context).await
            }),
    );
    let parent_task = dormant("test.dormant");
    let harness = open_tasks(storage.clone(), vec![parent_task.clone(), child_task.clone()]).await.harness;
    let root = harness.root(&context(), CreateOptions::default()).await.unwrap();
    let id = root.id;
    let create =
        move |tx: Tx| async move { tx.create_task(parent_task, Json::Null, TaskOptions::conversation(id)).await };
    let parent = root.commit(create, &context()).await.unwrap();
    let create =
        move |tx: Tx| async move { tx.create_task(child_task, Json::Null, TaskOptions::owned_by(parent)).await };
    let child = root.commit(create, &context()).await.unwrap();
    // Hold every commit that abort-marks the child, until released.
    let target = Arc::new(AtomicU64::new(child.0));
    let held = storage.hold_commits_where(move |writes| {
        writes.iter().any(|write| {
            matches!(write, StorageWrite::Task { value } if value.id.0 == target.load(Ordering::SeqCst) && value.abort_requested)
        })
    });
    harness.resume().unwrap();
    entered.wait().await;
    let aborting_harness = harness.clone();
    let aborting = tokio::spawn(async move { aborting_harness.abort_task(parent, &context()).await });
    held.entered().await;
    // The parent's request is durable, the cascade's mark of the child is not: its phase must not advance.
    advance.resolve();
    for _ in 0..5 {
        flush().await;
    }
    assert!(!ran_next.load(Ordering::SeqCst));
    held.release();
    assert_eq!(outcome_status(&harness, child).await, "aborted");
    aborting.await.unwrap().unwrap();
    assert!(!ran_next.load(Ordering::SeqCst));
    harness.close(&context()).await.unwrap();
}

/// A Session over `storage` with an ownerless conversation holding a dormant task; returns the task.
async fn seed(storage: Arc<MemoryStorage>, options: TaskOptions) -> (pi_durable::session::Session, TaskId) {
    let session = create_session(storage, SessionOptions::default());
    let conversation = session
        .commit(|tx: Tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await }, &context());
    let id = conversation.await.unwrap().id;
    let options = TaskOptions { conversation_id: Some(id), ..options };
    let task = dormant("test.dormant");
    let created =
        session.commit(move |tx: Tx| async move { tx.create_task(task, Json::Null, options).await }, &context());
    let top = created.await.unwrap();
    (session, top)
}

async fn create_child(session: &pi_durable::session::Session, task: Task, options: TaskOptions) -> TaskId {
    session
        .commit(move |tx: Tx| async move { tx.create_task(task, Json::Null, options).await }, &context())
        .await
        .unwrap()
}

async fn record(storage: &MemoryStorage, id: TaskId) -> TaskRecord {
    storage.task(id, &context()).await.unwrap().unwrap()
}

#[tokio::test]
async fn lets_a_fail_fast_request_in_the_same_pass_override_a_restart_mark() {
    let storage = Arc::new(MemoryStorage::new());
    let (session, parent) = seed(storage.clone(), TaskOptions::conversation(ConversationId(0))).await;
    let failed = create_child(&session, dormant("test.dormant"), TaskOptions::owned_by(parent)).await;
    let child = create_child(&session, dormant("test.dormant"), TaskOptions::owned_by(parent)).await;
    // A restart-abandoned owner that waits failFast on a failed child and a live one.
    let waiting = TaskState::Waiting {
        checkpoint: Json::from(json!({ "phase": "work" })),
        on: vec![failed, child],
        policy: JoinPolicy::FailFast,
    };
    let owner = TaskRecord {
        abort_requested: true,
        abort_reason: Some(AbortReason::Restart),
        state: waiting,
        ..record(&storage, parent).await
    };
    let failure = TaskOutcome::Failed { error: TaskOutcomeError::new("failed"), result: None };
    let failed_record =
        TaskRecord { state: TaskState::Terminal { outcome: failure }, ..record(&storage, failed).await };
    let writes = [StorageWrite::Task { value: owner }, StorageWrite::Task { value: failed_record }];
    storage.commit(&writes, &|_| {}, &context()).await.unwrap();
    // The child's definition is missing: a restart mark would keep it waiting, a request orphans it.
    let harness = open_tasks(storage, Vec::new()).await.harness;
    harness.resume().unwrap();
    eventually(|| async {
        let task = harness.get_task(child, &context()).await.unwrap();
        task.is_some_and(|task| task.state.status() == TaskStatus::Terminal)
    })
    .await;
    let outcome = harness.get_task(child, &context()).await.unwrap().unwrap().state.outcome().cloned();
    assert_eq!(outcome, Some(TaskOutcome::Orphaned { reason: "missing_task".into() }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn upgrades_restart_marks_down_the_tree_when_an_abort_is_requested_above_them() {
    let storage = Arc::new(MemoryStorage::new());
    let (session, top) = seed(storage.clone(), TaskOptions::conversation(ConversationId(0))).await;
    let restartable = TaskOptions { abandon_on_restart: true, ..TaskOptions::owned_by(top) };
    let middle = create_child(&session, dormant("test.dormant"), restartable).await;
    let leaf = create_child(&session, dormant("test.missing"), TaskOptions::owned_by(middle)).await;
    // Only the dormant kind is registered: the leaf waits for its definition under the restart cascade.
    let harness = open_tasks(storage, vec![dormant("test.dormant")]).await.harness;
    harness.resume().unwrap();
    eventually(|| async {
        let task = harness.get_task(leaf, &context()).await.unwrap();
        task.is_some_and(|task| task.abort_reason == Some(AbortReason::Restart))
    })
    .await;
    eventually(|| async {
        let inspection = harness.inspect(&context()).await.unwrap();
        let found = inspection.tasks.into_iter().find(|task| task.record.id == leaf);
        found.is_some_and(|task| matches!(task.state, TaskInspectionState::Blocked { .. }))
    })
    .await;
    let status = harness.get_task(leaf, &context()).await.unwrap().unwrap().state.status();
    assert_ne!(status, TaskStatus::Terminal);
    // A request on the top task reaches the leaf two levels down, which is then orphaned.
    harness.abort_task(top, &context()).await.unwrap();
    for (id, status) in [(leaf, "orphaned"), (middle, "aborted"), (top, "aborted")] {
        assert_eq!(outcome_status(&harness, id).await, status);
    }
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn marks_a_flagged_task_that_holds_its_outcome_aborting_its_owned_work_and_keeps_the_outcome() {
    let storage = Arc::new(MemoryStorage::new());
    let options = TaskOptions { abandon_on_restart: true, ..TaskOptions::conversation(ConversationId(0)) };
    let (session, holder) = seed(storage.clone(), options).await;
    let child = create_child(&session, dormant("test.dormant"), TaskOptions::owned_by(holder)).await;
    let completing = TaskState::Completing { outcome: TaskOutcome::Completed { result: Some(Json::Null) } };
    let value = TaskRecord { state: completing, ..record(&storage, holder).await };
    storage.commit(&[StorageWrite::Task { value }], &|_| {}, &context()).await.unwrap();
    let harness = open_tasks(storage, vec![dormant("test.dormant")]).await.harness;
    harness.resume().unwrap();
    assert_eq!(outcome_status(&harness, child).await, "aborted");
    let settled = harness.wait_for_task(holder, &context()).await.unwrap();
    assert_eq!(settled.abort_reason, Some(AbortReason::Restart));
    assert_eq!(settled.state.outcome().unwrap().status(), "completed");
    harness.close(&context()).await.unwrap();
}
