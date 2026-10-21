//! The upstream "waiting" cases for waits that close a cycle (#10411, #10533 by @odysseus0).

use std::collections::HashMap;

use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;

use super::nodes::*;
use super::*;
use crate::support::Deferred;

/// Task ids by name, set by the commit that creates them and read when a script runs.
type Named = Arc<Mutex<HashMap<&'static str, TaskId>>>;

fn id_of(ids: &Named, name: &str) -> TaskId {
    *ids.lock().unwrap().get(name).unwrap_or_else(|| panic!("no task {name}"))
}

/// Create the tasks `names` in one commit, so each exists before any of them runs.
async fn create_all(root: &Conversation, nodes: &Nodes, ids: &Named, names: &'static [&'static str]) {
    let (task, found, conversation) = (nodes.task.clone(), ids.clone(), root.id);
    let create = move |tx: Tx| async move {
        for name in names {
            let options = TaskOptions::conversation(conversation);
            let id = tx.create_task(task.clone(), named(name), options).await?;
            found.lock().unwrap().insert(*name, id);
        }
        Ok(())
    };
    root.commit(create, &context()).await.unwrap();
}

/// Script `name` to wait on the tasks `targets`, looked up when it runs.
fn waits_on(nodes: &Nodes, ids: &Named, name: &str, targets: &'static [&'static str]) {
    let ids = ids.clone();
    nodes.run(name, move |runtime, context| {
        let on: Vec<TaskId> = targets.iter().map(|target| id_of(&ids, target)).collect();
        async move { runtime.commit(move |_, _| async move { Ok(wait_on(on, JoinPolicy::AllSettled, 1)) }, &context).await }
    });
}

async fn sorted_outcomes(harness: &Harness, tasks: &[TaskId]) -> Vec<&'static str> {
    let mut outcomes = Vec::new();
    for task in tasks {
        outcomes.push(outcome_of(harness, *task).await);
    }
    outcomes.sort_unstable();
    outcomes
}

#[tokio::test]
async fn rejects_the_wait_that_closes_a_cycle() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    waits_on(&nodes, &ids, "a", &["b"]);
    waits_on(&nodes, &ids, "b", &["a"]);
    create_all(&root, &nodes, &ids, &["a", "b"]).await;
    // a -> b and b -> a are waits, and whichever comes second closes the cycle.
    let tasks = [id_of(&ids, "a"), id_of(&ids, "b")];
    assert_eq!(sorted_outcomes(&harness, &tasks).await, ["completed", "faulted"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_the_wait_that_closes_a_cycle_through_a_held_owner() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    let child = spawn_child(&nodes, "held", "child", false);
    waits_on(&nodes, &ids, "child", &["other"]);
    waits_on(&nodes, &ids, "other", &["held"]);
    create_all(&root, &nodes, &ids, &["held", "other"]).await;
    // held -> child is a hold; child -> other -> held are waits, and whichever wait comes second closes the cycle.
    assert_eq!(outcome_of(&harness, id_of(&ids, "held")).await, "completed");
    let tasks = [ids_of(&child), id_of(&ids, "other")];
    assert_eq!(sorted_outcomes(&harness, &tasks).await, ["completed", "faulted"]);
    harness.close(&context()).await.unwrap();
}

fn ids_of(found: &Ids) -> TaskId {
    ids(found)[0]
}

#[tokio::test]
async fn rejects_a_cycle_through_holds_two_levels_deep() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids_by_name = Named::default();
    let child = spawn_child(&nodes, "held", "child", false);
    let grandchild = spawn_child(&nodes, "child", "grandchild", false);
    waits_on(&nodes, &ids_by_name, "grandchild", &["other"]);
    waits_on(&nodes, &ids_by_name, "other", &["held"]);
    create_all(&root, &nodes, &ids_by_name, &["held", "other"]).await;
    // held -> child -> grandchild are holds; grandchild -> other -> held are waits.
    eventually(|| async { !ids(&grandchild).is_empty() }).await;
    let tasks = [ids_of(&grandchild), id_of(&ids_by_name, "other")];
    assert_eq!(sorted_outcomes(&harness, &tasks).await, ["completed", "faulted"]);
    assert_eq!(outcome_of(&harness, ids_of(&child)).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn lets_a_background_task_wait_on_the_owner_of_its_conversation() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    waits_on(&nodes, &ids, "background", &["owner"]);
    let (task, found, conversation) = (nodes.task.clone(), ids.clone(), root.id);
    let create = move |tx: Tx| async move {
        let options = TaskOptions::conversation(conversation);
        let owner = tx.create_task(task.clone(), named("owner"), options).await?;
        found.lock().unwrap().insert("owner", owner);
        let inner = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
        let options = TaskOptions { background: true, ..TaskOptions::conversation(inner.id) };
        tx.create_task(task, named("background"), options).await
    };
    let background = root.commit(create, &context()).await.unwrap();
    // background -> owner is a wait but owner -> background is no hold, so there is no cycle.
    until_status(&harness, background, TaskStatus::Waiting).await;
    nodes.open_ok("owner");
    assert_eq!(outcome_of(&harness, background).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_cycle_of_a_hold_and_two_waits_that_each_look_reasonable() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    let reviewer_waits = Deferred::default();
    // The orchestrator spawns a worker and finishes; the running worker holds it as completing.
    let worker = spawn_child(&nodes, "orchestrator", "worker", false);
    // The reviewer waits for the orchestrator's result.
    let (found, signal) = (ids.clone(), reviewer_waits.clone());
    nodes.run("reviewer", move |runtime, context| {
        let (orchestrator, signal) = (id_of(&found, "orchestrator"), signal.clone());
        async move {
            let wait = move |_: Tx, _| async move { Ok(wait_on(vec![orchestrator], JoinPolicy::AllSettled, 1)) };
            runtime.commit(wait, &context).await?;
            signal.resolve();
            Ok(())
        }
    });
    // The worker waits for the reviewer, closing the cycle.
    let (found, gate) = (ids.clone(), reviewer_waits.clone());
    nodes.run("worker", move |runtime, context| {
        let (found, gate) = (found.clone(), gate.clone());
        async move {
            gate.wait().await;
            let reviewer = id_of(&found, "reviewer");
            runtime
                .commit(move |_, _| async move { Ok(wait_on(vec![reviewer], JoinPolicy::AllSettled, 1)) }, &context)
                .await
        }
    });
    create_all(&root, &nodes, &ids, &["orchestrator"]).await;
    let orchestrator = id_of(&ids, "orchestrator");
    until_status(&harness, orchestrator, TaskStatus::Completing).await;
    create_all(&root, &nodes, &ids, &["reviewer"]).await;
    let reviewer = id_of(&ids, "reviewer");
    let worker = ids_of(&worker);
    let outcomes = [outcome_of(&harness, orchestrator).await, outcome_of(&harness, worker).await];
    assert_eq!((outcomes, outcome_of(&harness, reviewer).await), (["completed", "faulted"], "completed"));
    let cycle = format!("{worker} -> {reviewer} -> {orchestrator} -> {worker} is a wait cycle");
    assert_faulted(&harness, worker, &cycle).await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn allows_a_diamond_of_waits() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    // A diamond: top waits on left and right, both wait on bottom. No cycle.
    waits_on(&nodes, &ids, "top", &["left", "right"]);
    waits_on(&nodes, &ids, "left", &["bottom"]);
    waits_on(&nodes, &ids, "right", &["bottom"]);
    create_all(&root, &nodes, &ids, &["bottom", "left", "right", "top"]).await;
    nodes.open_ok("bottom");
    assert_eq!(outcome_of(&harness, id_of(&ids, "top")).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn follows_no_wait_of_an_abort_marked_task_which_waits_only_for_its_owned_work() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    // b owns c and waits on a; aborting b leaves c's cleanup pending, so b waits only for c.
    let (found, spawner) = (ids.clone(), nodes.clone());
    nodes.run("b", move |runtime, context| {
        let (a, nodes) = (id_of(&found, "a"), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                spawn(&tx, &nodes, owner, "c").await?;
                Ok(wait_on(vec![a], JoinPolicy::AllSettled, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    nodes.slow_abort("c");
    let a_waits = Deferred::default();
    let (found, gate) = (ids.clone(), a_waits.clone());
    nodes.run("a", move |runtime, context| {
        let (found, gate) = (found.clone(), gate.clone());
        async move {
            gate.wait().await;
            let b = id_of(&found, "b");
            runtime.commit(move |_, _| async move { Ok(wait_on(vec![b], JoinPolicy::AllSettled, 1)) }, &context).await
        }
    });
    create_all(&root, &nodes, &ids, &["a", "b"]).await;
    let (a, b) = (id_of(&ids, "a"), id_of(&ids, "b"));
    until_status(&harness, b, TaskStatus::Waiting).await;
    harness.abort_task(b, &context()).await.unwrap();
    eventually(|| async { nodes.logged("abort:c") }).await;
    // a -> b would close a -> b -> a through b's stale wait; b now waits only for c.
    a_waits.resolve();
    until_status(&harness, a, TaskStatus::Waiting).await;
    nodes.open_ok("abort.c");
    assert_eq!(outcome_of(&harness, b).await, "aborted");
    assert_eq!(outcome_of(&harness, a).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_a_longer_cycle_of_waits() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let ids = Named::default();
    waits_on(&nodes, &ids, "x", &["y"]);
    waits_on(&nodes, &ids, "y", &["z"]);
    waits_on(&nodes, &ids, "z", &["x"]);
    create_all(&root, &nodes, &ids, &["x", "y", "z"]).await;
    let tasks = [id_of(&ids, "x"), id_of(&ids, "y"), id_of(&ids, "z")];
    assert_eq!(sorted_outcomes(&harness, &tasks).await, ["completed", "completed", "faulted"]);
    harness.close(&context()).await.unwrap();
}
