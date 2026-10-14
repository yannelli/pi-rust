//! Upstream `describe("boundaries")` and `describe("recovery")`.

use std::sync::{Arc, Mutex};

use pi_durable::session::{SessionOptions, Tx, create_session};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::nodes::*;
use super::support::context;
use super::support::task_support::eventually;
use super::waiting::parent_of;

#[tokio::test]
async fn keeps_background_work_through_conversation_abort_background_true_aborts_it_and_waits() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let foreground = start(&root, &nodes, "foreground").await;
    let found = Ids::default();
    let (sink, spawner) = (found.clone(), nodes.clone());
    nodes.run("background", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let child = spawn(&tx, &nodes, owner, "child").await?;
                let conversation = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                let options = TaskOptions::conversation(conversation.id);
                let below = tx.create_task(nodes.task.clone(), named("below"), options).await?;
                sink.lock().unwrap().extend([child, below]);
                Ok(wait_on(vec![child], JoinPolicy::AllSettled, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    let options = TaskOptions { background: true, ..TaskOptions::conversation(root.id) };
    let background = start_with(&root, &nodes, "background", options).await;
    eventually(|| async { nodes.logged("run:below") && nodes.logged("run:child") }).await;
    root.abort(&context(), false).await.unwrap();
    assert_eq!(outcome_of(&harness, foreground).await, "aborted");
    let tree = [background, ids(&found)[0], ids(&found)[1]];
    for id in tree {
        assert_ne!(status(&harness, id).await, TaskStatus::Terminal);
    }
    root.abort(&context(), true).await.unwrap();
    for id in tree {
        assert_eq!(status(&harness, id).await, TaskStatus::Terminal);
    }
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn never_cascades_from_a_terminal_owner_an_aborted_subagent_s_conversation_runs_new_work_normally() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let conversation = Arc::new(Mutex::new(None));
    let sink = conversation.clone();
    nodes.run("agent", move |runtime, context| {
        let sink = sink.clone();
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                *sink.lock().unwrap() = Some(created.id);
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    resume_until_aborted(&nodes, "agent");
    let agent = start(&root, &nodes, "agent").await;
    eventually(|| async { nodes.logged("resume:agent") }).await;
    harness.abort_task(agent, &context()).await.unwrap();
    assert_eq!(outcome_of(&harness, agent).await, "aborted");
    let id = conversation.lock().unwrap().unwrap();
    let child = harness.conversation(id, &context()).await.unwrap().unwrap();
    let question = start(&child, &nodes, "question").await;
    nodes.open_ok("question");
    assert_eq!(outcome_of(&harness, question).await, "completed");
    harness.close(&context()).await.unwrap();
}

pub async fn sqlite(directory: &tempfile::TempDir) -> Arc<dyn Storage> {
    let path = directory.path().join("session.sqlite");
    Arc::new(open_sqlite_storage(&path.to_string_lossy()).await.unwrap())
}

/// A replacement of a seeded record's `abortRequested` (when set) and `state`.
pub struct Patch {
    pub id: TaskId,
    pub abort_requested: Option<bool>,
    pub state: Value,
}

pub fn patch(id: TaskId, state: Value) -> Patch {
    Patch { id, abort_requested: None, state }
}

/// Write records a crash could leave, without a Harness: `parent` with children named `children`, then the patches
/// `edit` returns replace states through the internal task write.
pub async fn seed(
    storage: Arc<dyn Storage>,
    nodes: &Nodes,
    children: &'static [&'static str],
    edit: impl FnOnce(TaskId, &[TaskId]) -> Vec<Patch>,
) -> (TaskId, Vec<TaskId>) {
    let session = create_session(storage, SessionOptions::default());
    let spawner = nodes.clone();
    let stage = move |tx: Tx| async move {
        let root = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        let parent = tx.create_task(spawner.task.clone(), named("parent"), TaskOptions::conversation(root.id)).await?;
        let mut ids = Vec::new();
        for name in children {
            ids.push(spawn(&tx, &spawner, parent, name).await?);
        }
        Ok((parent, ids))
    };
    let (parent, ids) = session.commit(stage, &context()).await.unwrap();
    for Patch { id, abort_requested, state } in edit(parent, &ids) {
        let state: TaskState = serde_json::from_value(state).unwrap();
        let write = move |tx: Tx| async move {
            let record = tx.task(id).await?.unwrap();
            let abort_requested = abort_requested.unwrap_or(record.abort_requested);
            tx.set_task(TaskRecord { abort_requested, state, ..record })
        };
        session.commit(write, &context()).await.unwrap();
    }
    session.close(&context()).await.unwrap();
    (parent, ids)
}

fn completed_state() -> Value {
    json!({ "status": "terminal", "outcome": { "status": "completed", "result": "done" } })
}

fn waiting(on: &[TaskId], policy: &str) -> Value {
    json!({ "status": "waiting", "checkpoint": { "phase": "resume", "round": 1 }, "on": on, "policy": policy })
}

#[tokio::test]
async fn resumes_a_parent_whose_awaited_children_finished_before_the_crash() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let (parent, children) = seed(sqlite(&directory).await, &nodes, &["c1"], |parent, children| {
        vec![patch(children[0], completed_state()), patch(parent, waiting(children, "allSettled"))]
    })
    .await;
    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(nodes.log(), ["resume:parent"]);
    assert_eq!(children.len(), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn marks_fail_fast_siblings_a_crash_left_unmarked() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let failed = json!({ "status": "terminal", "outcome": { "status": "failed", "error": { "message": "declined" } } });
    let (parent, children) = seed(sqlite(&directory).await, &nodes, &["c1", "c2"], |parent, children| {
        vec![patch(children[0], failed), patch(parent, waiting(children, "failFast"))]
    })
    .await;
    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    assert_eq!(outcome_of(&harness, children[1]).await, "aborted");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert!(!nodes.logged("run:c2"));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn finalizes_a_held_outcome_whose_work_drained_before_the_crash_and_keeps_one_whose_work_lives() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let held = json!({ "status": "completing", "outcome": { "status": "completed", "result": "parent" } });
    let held_patch = held.clone();
    let (parent, children) =
        seed(sqlite(&directory).await, &nodes, &["c1"], move |parent, _| vec![patch(parent, held_patch)]).await;
    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    eventually(|| async { nodes.logged("run:c1") }).await;
    assert_eq!(serde_json::to_value(state(&harness, parent).await).unwrap(), held);
    harness.close(&context()).await.unwrap();

    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    nodes.open_ok("c1");
    assert_eq!(outcome_of(&harness, children[0]).await, "completed");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    harness.close(&context()).await.unwrap();

    let drained = tempfile::tempdir().unwrap();
    let (second, _) = seed(sqlite(&drained).await, &nodes, &["c1"], move |parent, children| {
        vec![patch(children[0], completed_state()), patch(parent, held)]
    })
    .await;
    let Opened { harness, .. } = open_nodes_in(sqlite(&drained).await, &nodes).await;
    assert_eq!(outcome_of(&harness, second).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn resumes_a_bottom_up_abort_a_crash_interrupted_before_the_cascade() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let (parent, children) = seed(sqlite(&directory).await, &nodes, &["c1"], |parent, children| {
        vec![Patch { id: parent, abort_requested: Some(true), state: waiting(children, "allSettled") }]
    })
    .await;
    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    assert_eq!(outcome_of(&harness, parent).await, "aborted");
    assert_eq!(outcome_of(&harness, children[0]).await, "aborted");
    assert_eq!(nodes.lines("abort:"), ["abort:c1", "abort:parent"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reopens_a_checkout_waiting_on_live_payments_and_finishes_it() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let (found, outcomes) = parent_of(&nodes, "checkout", &["p1", "p2"], JoinPolicy::FailFast);
    let Opened { harness, root, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    let parent = start(&root, &nodes, "checkout").await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    eventually(|| async { nodes.logged("run:p1") && nodes.logged("run:p2") }).await;
    harness.close(&context()).await.unwrap();

    let Opened { harness, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    let TaskState::Waiting { on, .. } = state(&harness, parent).await else { panic!("not waiting") };
    *found.lock().unwrap() = on;
    nodes.open_ok("p1");
    nodes.open_ok("p2");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(*outcomes.lock().unwrap(), ["completed", "completed"]);
    harness.close(&context()).await.unwrap();
}
