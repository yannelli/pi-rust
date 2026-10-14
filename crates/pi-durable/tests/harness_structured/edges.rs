//! Upstream `describe("conversation abort and boundaries")` and `describe("recovery through ownership edges")`.

use std::sync::{Arc, Mutex};

use pi_durable::harness::*;
use pi_durable::session::{SessionEnd, SessionOptions, Tx, create_session};
use pi_durable::tasks::TaskOptions;
use pi_durable::*;

use super::nodes::*;
use super::recovery::sqlite;
use super::support::task_support::{eventually, open_tasks};
use super::support::{ControlledStorage, context};
use super::waiting::parent_of;

fn background_in(conversation: &Conversation) -> TaskOptions {
    TaskOptions { background: true, ..TaskOptions::conversation(conversation.id) }
}

fn idle(conversation: &Conversation) -> impl Future<Output = DurableResult<()>> + Send + 'static {
    let conversation = conversation.clone();
    async move { conversation.wait_for_idle(&context()).await }
}

#[tokio::test]
async fn marks_a_held_completed_task_with_conversation_abort_the_work_below_is_aborted_the_outcome_stays() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let child = spawn_child(&nodes, "held", "child", false);
    let held = start(&root, &nodes, "held").await;
    until_status(&harness, held, TaskStatus::Completing).await;
    root.abort(&context(), false).await.unwrap();
    assert_eq!(outcome_of(&harness, ids(&child)[0]).await, "aborted");
    let record = harness.wait_for_task(held, &context()).await.unwrap();
    assert_eq!(record.state.outcome().map(TaskOutcome::status), Some("completed"));
    assert!(record.abort_requested);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn marks_and_awaits_only_the_background_work_reached_when_background_true_is_admitted() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    nodes.slow_abort("first");
    let first = start_with(&root, &nodes, "first", background_in(&root)).await;
    eventually(|| async { nodes.logged("run:first") }).await;
    let aborter = root.clone();
    let aborting = tokio::spawn(async move { aborter.abort(&context(), true).await });
    eventually(|| marked(&harness, first)).await;
    let later = start_with(&root, &nodes, "later", background_in(&root)).await;
    nodes.open_ok("abort.first");
    aborting.await.unwrap().unwrap();
    assert_ne!(status(&harness, later).await, TaskStatus::Terminal);
    assert!(!marked(&harness, later).await);
    harness.abort_task(later, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn stops_a_cascade_at_an_unmarked_background_task_but_not_at_a_marked_one() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    nodes.slow_abort("owner");
    let (id, task) = (root.id, nodes.task.clone());
    let stage = move |tx: Tx| async move {
        let owner = tx.create_task(task.clone(), named("owner"), TaskOptions::conversation(id)).await?;
        let outer = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let options = TaskOptions { background: true, ..TaskOptions::conversation(outer.id) };
        let background = tx.create_task(task, named("background"), options).await?;
        let inner = tx.create_conversation(ConversationOwnership::Task { task_id: background }).await?;
        Ok((owner, outer.id, background, inner.id))
    };
    let (owner, outer, background, inner) = root.commit(stage, &context()).await.unwrap();
    eventually(|| async { nodes.logged("run:owner") && nodes.logged("run:background") }).await;
    // The owner's abort handler starts at once: background work is not its ordinary owned work.
    harness.abort_task(owner, &context()).await.unwrap();
    eventually(|| async { nodes.logged("abort:owner") }).await;
    let inner = harness.conversation(inner, &context()).await.unwrap().unwrap();
    let outer = harness.conversation(outer, &context()).await.unwrap().unwrap();
    let shielded = start(&inner, &nodes, "shielded").await;
    let exposed = start(&outer, &nodes, "exposed").await;
    assert_eq!(outcome_of(&harness, exposed).await, "aborted");
    assert!(!marked(&harness, shielded).await);
    // Marked directly, the background task cascades into its own subtree.
    harness.abort_task(background, &context()).await.unwrap();
    assert_eq!(outcome_of(&harness, shielded).await, "aborted");
    assert_eq!(outcome_of(&harness, background).await, "aborted");
    nodes.open_ok("abort.owner");
    assert_eq!(outcome_of(&harness, owner).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_the_harness_on_a_failed_finalization_which_reopening_applies() {
    let storage = Arc::new(ControlledStorage::default());
    let nodes = Nodes::new();
    let Opened { harness, root, reports, .. } = open_nodes_in(storage.clone(), &nodes).await;
    spawn_child(&nodes, "parent", "child", false);
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Completing).await;
    let finalizes = move |writes: &[StorageWrite]| {
        writes.iter().any(|write| {
            matches!(write, StorageWrite::Task { value } if value.id == parent && value.state.status() == TaskStatus::Terminal)
        })
    };
    storage.fail_commit_where(finalizes, DurableError::error("disk gone"));
    nodes.open_ok("child");
    let end = harness.closed().await;
    assert!(matches!(&end, SessionEnd::Failed(error) if error.to_string() == "disk gone"), "{end:?}");
    let reports: Vec<String> = reports.lock().unwrap().iter().map(ToString::to_string).collect();
    assert_eq!(reports, ["disk gone"]);
    harness.close(&context()).await.unwrap();
    storage.reopen();
    let Opened { harness, .. } = open_nodes_in(storage.clone(), &nodes).await;
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn shows_an_abort_marked_owner_waiting_for_work_in_its_owned_conversation_before_resume_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let session = create_session(sqlite(&directory).await, SessionOptions::default());
    let task = nodes.task.clone();
    let stage = move |tx: Tx| async move {
        let conversation = tx.create_conversation(ConversationOwnership::Ownerless).await?;
        let owner = tx.create_task(task.clone(), named("owner"), TaskOptions::conversation(conversation.id)).await?;
        let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let inner = tx.create_task(task, named("inner"), TaskOptions::conversation(child.id)).await?;
        Ok((owner, inner))
    };
    let (owner, inner) = session.commit(stage, &context()).await.unwrap();
    let mark = move |tx: Tx| async move {
        let record = tx.task(owner).await?.unwrap();
        tx.set_task(TaskRecord { abort_requested: true, ..record })
    };
    session.commit(mark, &context()).await.unwrap();
    session.close(&context()).await.unwrap();

    let harness = open_tasks(sqlite(&directory).await, vec![nodes.task.clone()]).await.harness;
    let inspection = harness.inspect(&context()).await.unwrap();
    let inspected = inspection.tasks.into_iter().find(|task| task.record.id == owner).map(|task| task.state);
    assert_eq!(inspected, Some(TaskInspectionState::Waiting { on: vec![inner] }));
    harness.resume().unwrap();
    assert_eq!(outcome_of(&harness, owner).await, "aborted");
    assert_eq!(nodes.lines("abort:"), ["abort:inner", "abort:owner"]);
    harness.close(&context()).await.unwrap();
}

/// Script `name` to create a conversation it owns (holding `inner` when set), then go on in round 1.
fn owning_conversation(nodes: &Nodes, name: &str, inner: Option<&'static str>) -> Arc<Mutex<Option<ConversationId>>> {
    let conversation = Arc::new(Mutex::new(None));
    let (sink, task) = (conversation.clone(), nodes.task.clone());
    nodes.run(name, move |runtime, context| {
        let (sink, task) = (sink.clone(), task.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                *sink.lock().unwrap() = Some(created.id);
                if let Some(inner) = inner {
                    tx.create_task(task, named(inner), TaskOptions::conversation(created.id)).await?;
                }
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    conversation
}

#[tokio::test]
async fn keeps_the_root_busy_after_reopen_for_work_below_a_child_task_s_owned_conversation() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let conversation = owning_conversation(&nodes, "child", Some("inner"));
    parent_of(&nodes, "parent", &["child"], JoinPolicy::AllSettled);
    let Opened { harness, root, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { nodes.logged("run:inner") }).await;
    harness.close(&context()).await.unwrap();

    let Opened { harness, root, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    assert!(!settled(idle(&root)).await);
    nodes.open_ok("inner");
    root.wait_for_idle(&context()).await.unwrap();
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert!(conversation.lock().unwrap().is_some());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_finished_background_ancestor_a_boundary_after_reopen_which_background_true_crosses() {
    let directory = tempfile::tempdir().unwrap();
    let nodes = Nodes::new();
    let conversation = owning_conversation(&nodes, "child", None);
    parent_of(&nodes, "background", &["child"], JoinPolicy::AllSettled);
    let Opened { harness, root, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    let background = start_with(&root, &nodes, "background", background_in(&root)).await;
    harness.wait_for_task(background, &context()).await.unwrap();
    harness.close(&context()).await.unwrap();

    let Opened { harness, root, .. } = open_nodes_in(sqlite(&directory).await, &nodes).await;
    let id = conversation.lock().unwrap().unwrap();
    let below = start(&harness.conversation(id, &context()).await.unwrap().unwrap(), &nodes, "below").await;
    eventually(|| async { nodes.logged("run:below") }).await;
    root.wait_for_idle(&context()).await.unwrap();
    root.abort(&context(), false).await.unwrap();
    assert_eq!(status(&harness, below).await, TaskStatus::Running);
    root.abort(&context(), true).await.unwrap();
    assert_eq!(outcome_of(&harness, below).await, "aborted");
    harness.close(&context()).await.unwrap();
}
