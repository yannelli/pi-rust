//! Upstream `describe("completing")` and `describe("abort order")`.

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::nodes::*;
use super::support::context;
use super::support::task_support::eventually;

async fn inspected(harness: &Harness, id: TaskId) -> Option<TaskInspectionState> {
    let inspection = harness.inspect(&context()).await.unwrap();
    inspection.tasks.into_iter().find(|task| task.record.id == id).map(|task| task.state)
}

fn idle(conversation: &Conversation) -> impl Future<Output = DurableResult<()>> + Send + 'static {
    let conversation = conversation.clone();
    async move { conversation.wait_for_idle(&context()).await }
}

#[tokio::test]
async fn holds_a_finished_task_until_its_owned_work_drains_waiters_and_task_documents_wait_for_the_final_commit() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let child = Ids::default();
    let (sink, spawner) = (child.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                tx.doc(&TASK_NOTES, owner).await?.set(&Json::from(json!({ "text": "notes" })))?;
                let id = spawn(&tx, &nodes, owner, "child").await?;
                sink.lock().unwrap().push(id);
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Completing).await;
    let held = serde_json::to_value(state(&harness, parent).await).unwrap();
    assert_eq!(held, json!({ "status": "completing", "outcome": { "status": "completed", "result": "parent" } }));
    let waiter = tokio::spawn(wait_task(&harness, parent));
    assert!(!settled(wait_task(&harness, parent)).await);
    let notes = harness.snapshot(&TASK_NOTES, parent, &context()).await.unwrap();
    assert_eq!(notes.map(|notes| notes.to_serde()), Some(json!({ "text": "notes" })));
    assert_eq!(inspected(&harness, parent).await, Some(TaskInspectionState::Completing));
    assert!(!settled(idle(&root)).await);
    nodes.open_ok("child");
    let settled_parent = waiter.await.unwrap().unwrap();
    let outcome = serde_json::to_value(settled_parent.state.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "completed", "result": "parent" }));
    assert!(harness.snapshot(&TASK_NOTES, parent, &context()).await.unwrap().is_none());
    root.wait_for_idle(&context()).await.unwrap();
    assert_eq!(ids(&child).len(), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_holding_for_ordinary_work_created_during_the_hold() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let conversation = Arc::new(Mutex::new(None));
    let (sink, task) = (conversation.clone(), nodes.task.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, task) = (sink.clone(), task.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let created = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                *sink.lock().unwrap() = Some(created.id);
                tx.create_task(task, named("first"), TaskOptions::conversation(created.id)).await?;
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Completing).await;
    let id = conversation.lock().unwrap().unwrap();
    let child = harness.conversation(id, &context()).await.unwrap().unwrap();
    start(&child, &nodes, "second").await;
    nodes.open_ok("first");
    eventually(|| async { nodes.logged("run:second") }).await;
    assert_eq!(status(&harness, parent).await, TaskStatus::Completing);
    nodes.open_ok("second");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    harness.close(&context()).await.unwrap();
}

async fn aborts_the_work_below_then_finishes_with_the_held_outcome(throw: bool) {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let child = spawn_child(&nodes, "parent", "child", false);
    nodes.resume("parent", move |runtime, context, _| async move {
        if throw {
            return Err(DurableError::error("parent threw"));
        }
        runtime.commit(|_, _| async { Ok(end(Ending::Failed, "parent")) }, &context).await
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { !ids(&child).is_empty() }).await;
    assert_eq!(outcome_of(&harness, ids(&child)[0]).await, "aborted");
    assert_eq!(outcome_of(&harness, parent).await, if throw { "faulted" } else { "failed" });
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn aborts_the_work_below_a_held_failure_then_finishes_with_the_held_outcome() {
    aborts_the_work_below_then_finishes_with_the_held_outcome(false).await;
}

#[tokio::test]
async fn aborts_the_work_below_a_held_scheduler_fault_then_finishes_with_the_held_outcome() {
    aborts_the_work_below_then_finishes_with_the_held_outcome(true).await;
}

#[tokio::test]
async fn only_marks_a_completing_task_when_aborted_the_work_below_is_aborted_the_held_outcome_stays() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let child = spawn_child(&nodes, "parent", "child", false);
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Completing).await;
    assert_eq!(harness.abort_task(parent, &context()).await.unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome_of(&harness, ids(&child)[0]).await, "aborted");
    let settled_parent = harness.wait_for_task(parent, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled_parent.state.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "completed", "result": "parent" }));
    assert!(settled_parent.abort_requested);
    assert!(!nodes.logged("abort:parent"));
    assert_eq!(harness.abort_task(parent, &context()).await.unwrap(), AbortTaskResult::Terminal);
    harness.close(&context()).await.unwrap();
}

/// Script `name` to spawn `child` and wait on it; its abort handler records the child's status first.
fn chain(nodes: &Nodes, name: &'static str, child: &'static str) -> Ids {
    let found = spawn_child(nodes, name, child, true);
    let (source, logger) = (found.clone(), nodes.clone());
    nodes.abort(name, move |runtime, context| {
        let (child, logger) = (ids(&source)[0], logger.clone());
        async move {
            let status = runtime.get_task(child, &context).await?.unwrap().state.status();
            logger.push(format!("{name} saw {}", status.as_str()));
            runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await
        }
    });
    found
}

#[tokio::test]
async fn runs_abort_handlers_bottom_up_across_three_levels_each_after_the_level_below_is_terminal() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let b = chain(&nodes, "a", "b");
    let c = chain(&nodes, "b", "c");
    let a = start(&root, &nodes, "a").await;
    eventually(|| async { nodes.logged("run:c") }).await;
    until_status(&harness, ids(&c)[0], TaskStatus::Running).await;
    assert_eq!(harness.abort_task(a, &context()).await.unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome_of(&harness, a).await, "aborted");
    let order: Vec<String> =
        nodes.log().into_iter().filter(|line| line.starts_with("abort:") || line.contains(" saw ")).collect();
    assert_eq!(order, ["abort:c", "abort:b", "b saw terminal", "abort:a", "a saw terminal"]);
    assert_eq!(ids(&b).len(), 1);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn does_not_wait_for_a_task_it_waits_on_but_does_not_own() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let other = start(&root, &nodes, "other").await;
    nodes.run_commit("parent", move || wait_on(vec![other], JoinPolicy::AllSettled, 1));
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    harness.abort_task(parent, &context()).await.unwrap();
    assert_eq!(outcome_of(&harness, parent).await, "aborted");
    assert_eq!(status(&harness, other).await, TaskStatus::Running);
    nodes.open_ok("other");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn reports_an_abort_marked_task_as_waiting_for_its_live_owned_work() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    nodes.slow_abort("child");
    let child = spawn_child(&nodes, "parent", "child", true);
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    harness.abort_task(parent, &context()).await.unwrap();
    eventually(|| async { nodes.logged("abort:child") }).await;
    let on = ids(&child);
    assert_eq!(inspected(&harness, parent).await, Some(TaskInspectionState::Waiting { on }));
    nodes.open_ok("abort.child");
    assert_eq!(outcome_of(&harness, parent).await, "aborted");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn faults_an_abort_handler_that_tries_to_wait() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    nodes.abort("parent", |runtime, context| async move {
        runtime.commit(|_, _| async { Ok(wait_on(Vec::new(), JoinPolicy::AllSettled, 1)) }, &context).await
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { nodes.logged("run:parent") }).await;
    harness.abort_task(parent, &context()).await.unwrap();
    assert_faulted(&harness, parent, "cannot wait").await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn orphans_a_blocked_task_only_after_its_owned_work_drained() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let (id, spawner) = (root.id, nodes.clone());
    let stage = move |tx: Tx| async move {
        let owner = tx.create_task(unregistered(), named("owner"), TaskOptions::conversation(id)).await?;
        Ok((owner, spawn(&tx, &spawner, owner, "child").await?))
    };
    let (owner, child) = root.commit(stage, &context()).await.unwrap();
    eventually(|| async { nodes.logged("run:child") }).await;
    assert_eq!(harness.abort_task(owner, &context()).await.unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome_of(&harness, child).await, "aborted");
    assert_eq!(outcome_json(&harness, owner).await, json!({ "status": "orphaned", "reason": "missing_task" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cascades_through_task_and_conversation_edges_bottom_up() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    spawn_child(&nodes, "a", "b", true);
    let x = Ids::default();
    let (sink, task) = (x.clone(), nodes.task.clone());
    nodes.run("b", move |runtime, context| {
        let (sink, task) = (sink.clone(), task.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let conversation = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                let id = tx.create_task(task, named("x"), TaskOptions::conversation(conversation.id)).await?;
                sink.lock().unwrap().push(id);
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    resume_until_aborted(&nodes, "b");
    let a = start(&root, &nodes, "a").await;
    eventually(|| async { nodes.logged("run:x") }).await;
    harness.abort_task(a, &context()).await.unwrap();
    assert_eq!(outcome_of(&harness, a).await, "aborted");
    assert_eq!(outcome_of(&harness, ids(&x)[0]).await, "aborted");
    assert_eq!(nodes.lines("abort:"), ["abort:x", "abort:b", "abort:a"]);
    harness.close(&context()).await.unwrap();
}
