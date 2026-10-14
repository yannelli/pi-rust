//! Upstream `describe("waiting")`.

use std::sync::{Arc, Mutex};

use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;

use super::nodes::*;
use super::support::context;
use super::support::task_support::eventually;

pub type Outcomes = Arc<Mutex<Vec<&'static str>>>;

/// Record the outcome statuses of `on` read by `runtime`, then complete as `name`.
async fn finish_with_outcomes(
    runtime: TaskRuntime,
    context: pi_chord::Context,
    on: Vec<TaskId>,
    sink: Outcomes,
    name: &'static str,
) -> DurableResult<()> {
    let outcomes = runtime.outcomes(&on, &context).await?;
    *sink.lock().unwrap() = outcomes.iter().map(TaskOutcome::status).collect();
    runtime.commit(move |_, _| async move { Ok(end(Ending::Completed, name)) }, &context).await
}

/// Script `parent` to spawn `children` in one commit and wait on them with `policy`; it records their outcomes.
pub fn parent_of(
    nodes: &Nodes,
    parent: &'static str,
    children: &'static [&'static str],
    policy: JoinPolicy,
) -> (Ids, Outcomes) {
    let (found, outcomes) = (Ids::default(), Outcomes::default());
    let (sink, spawner) = (found.clone(), nodes.clone());
    nodes.run(parent, move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                for name in children {
                    let id = spawn(&tx, &nodes, owner, name).await?;
                    sink.lock().unwrap().push(id);
                }
                Ok(wait_on(ids(&sink), policy, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    let (source, sink) = (found.clone(), outcomes.clone());
    nodes.resume(parent, move |runtime, context, _| {
        finish_with_outcomes(runtime, context, ids(&source), sink.clone(), parent)
    });
    (found, outcomes)
}

#[tokio::test]
async fn resumes_once_every_awaited_task_is_terminal_and_reads_their_outcomes_in_order_all_settled() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let (found, outcomes) = (Ids::default(), Outcomes::default());
    let (sink, spawner) = (found.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                for name in ["ok", "fails", "throws", "aborted"] {
                    let id = spawn(&tx, &nodes, owner, name).await?;
                    sink.lock().unwrap().push(id);
                }
                let orphan = tx.create_task(unregistered(), named("orphan"), TaskOptions::owned_by(owner)).await?;
                sink.lock().unwrap().push(orphan);
                Ok(wait_on(ids(&sink), JoinPolicy::AllSettled, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    let (source, sink) = (found.clone(), outcomes.clone());
    nodes.resume("parent", move |runtime, context, _| {
        finish_with_outcomes(runtime, context, ids(&source), sink.clone(), "parent")
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { ids(&found).len() == 5 }).await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    nodes.open_ok("ok");
    nodes.open("fails", Ending::Failed);
    nodes.open("throws", Ending::Throw);
    let found = ids(&found);
    harness.abort_task(found[3], &context()).await.unwrap();
    assert_eq!(harness.abort_task(found[4], &context()).await.unwrap(), AbortTaskResult::Marked);
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(*outcomes.lock().unwrap(), ["completed", "failed", "faulted", "aborted", "orphaned"]);
    // allSettled never marks siblings.
    assert_eq!(nodes.lines("abort:"), ["abort:aborted"]);
    harness.close(&context()).await.unwrap();
}

async fn fails_fast_when_a_child_ends(ending: Ending) {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let (_, outcomes) = parent_of(&nodes, "checkout", &["p1", "p2", "p3", "p4"], JoinPolicy::FailFast);
    let parent = start(&root, &nodes, "checkout").await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    nodes.open("p2", ending);
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    let failed = if ending == Ending::Throw { "faulted" } else { "failed" };
    assert_eq!(*outcomes.lock().unwrap(), ["aborted", failed, "aborted", "aborted"]);
    assert!(!marked(&harness, parent).await);
    let mut aborts = nodes.lines("abort:");
    aborts.sort();
    assert_eq!(aborts, ["abort:p1", "abort:p3", "abort:p4"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn fails_fast_when_a_child_ends_failed_its_live_siblings_are_aborted_the_parent_is_not() {
    fails_fast_when_a_child_ends(Ending::Failed).await;
}

#[tokio::test]
async fn fails_fast_when_a_child_ends_faulted_its_live_siblings_are_aborted_the_parent_is_not() {
    fails_fast_when_a_child_ends(Ending::Throw).await;
}

#[tokio::test]
async fn fails_fast_on_a_held_failure_before_the_failing_child_drains() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let spawner = nodes.clone();
    nodes.run("p1", move |runtime, context| {
        let nodes = spawner.clone();
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                spawn(&tx, &nodes, owner, "grandchild").await?;
                Ok(resume_at(1))
            };
            runtime.commit(change, &context).await
        }
    });
    nodes.resume("p1", |runtime, context, _| async move {
        runtime.commit(|_, _| async { Ok(end(Ending::Failed, "p1")) }, &context).await
    });
    nodes.slow_abort("grandchild");
    let (found, outside) = (Ids::default(), Ids::default());
    let (sink, outside_sink, spawner) = (found.clone(), outside.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, outside_sink, nodes) = (sink.clone(), outside_sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                for name in ["p1", "p2"] {
                    let id = spawn(&tx, &nodes, owner, name).await?;
                    sink.lock().unwrap().push(id);
                }
                let outside = spawn(&tx, &nodes, owner, "outside").await?;
                outside_sink.lock().unwrap().push(outside);
                Ok(wait_on(ids(&sink), JoinPolicy::FailFast, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { ids(&found).len() == 2 }).await;
    let (found, outside) = (ids(&found), ids(&outside)[0]);
    // p1 holds `failed` while its grandchild's abort handler runs; p2 is already aborted.
    assert_eq!(outcome_of(&harness, found[1]).await, "aborted");
    let held = serde_json::to_value(state(&harness, found[0]).await).unwrap();
    assert_eq!((held["status"].as_str(), held["outcome"]["status"].as_str()), (Some("completing"), Some("failed")));
    assert_eq!(status(&harness, parent).await, TaskStatus::Waiting);
    nodes.open_ok("abort.grandchild");
    eventually(|| async { nodes.logged("resume:parent") }).await;
    // Only the other tasks in `on` are marked: not the failed one, not the parent, not a child outside `on`.
    assert_eq!(outcome_of(&harness, found[0]).await, "failed");
    let marks = [marked(&harness, found[0]).await, marked(&harness, found[1]).await, marked(&harness, outside).await];
    assert_eq!(marks, [false, true, false]);
    assert!(!marked(&harness, parent).await);
    assert_eq!(status(&harness, outside).await, TaskStatus::Running);
    // Finished, the parent holds for the child outside `on`.
    until_status(&harness, parent, TaskStatus::Completing).await;
    nodes.open_ok("outside");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn waits_with_all_settled_on_tasks_it_does_not_own_including_already_terminal_ones() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let done = start(&root, &nodes, "done").await;
    nodes.open_ok("done");
    harness.wait_for_task(done, &context()).await.unwrap();
    let live = start(&root, &nodes, "live").await;
    let outcomes = Outcomes::default();
    nodes.run_commit("parent", move || wait_on(vec![done, live], JoinPolicy::AllSettled, 1));
    let sink = outcomes.clone();
    nodes.resume("parent", move |runtime, context, _| {
        finish_with_outcomes(runtime, context, vec![done, live], sink.clone(), "parent")
    });
    let parent = start(&root, &nodes, "parent").await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    // A task it does not own is not its work: the parent may finish while it lives, but here it waits for it.
    nodes.open("live", Ending::Failed);
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(*outcomes.lock().unwrap(), ["completed", "failed"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn rejects_waits_on_itself_its_owner_a_missing_task_and_fail_fast_on_a_task_it_does_not_own() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let other = start(&root, &nodes, "other").await;
    let cases: [(&str, Option<TaskId>, JoinPolicy, &str); 3] = [
        ("self", None, JoinPolicy::AllSettled, "cannot wait on itself or its owner"),
        ("missing", Some(TaskId::from(999_999)), JoinPolicy::AllSettled, "does not exist"),
        ("foreign", Some(other), JoinPolicy::FailFast, "only on tasks it owns"),
    ];
    for (name, on, policy, message) in cases {
        nodes.run(name, move |runtime, context| {
            let on = on.unwrap_or(runtime.task_id());
            async move { runtime.commit(move |_, _| async move { Ok(wait_on(vec![on], policy, 1)) }, &context).await }
        });
        let id = start(&root, &nodes, name).await;
        assert_faulted(&harness, id, message).await;
    }
    // A child waiting on its owner could never resume.
    let child = Ids::default();
    nodes.run("child", |runtime, context| async move {
        let owner = runtime.get_task(runtime.task_id(), &context).await?.unwrap().owner.unwrap();
        runtime.commit(move |_, _| async move { Ok(wait_on(vec![owner], JoinPolicy::AllSettled, 1)) }, &context).await
    });
    let (sink, spawner) = (child.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                let child = spawn(&tx, &nodes, owner, "child").await?;
                sink.lock().unwrap().push(child);
                Ok(wait_on(vec![child], JoinPolicy::AllSettled, 1))
            };
            runtime.commit(change, &context).await
        }
    });
    let parent = start(&root, &nodes, "parent").await;
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_faulted(&harness, ids(&child)[0], "cannot wait on itself or its owner").await;
    nodes.open_ok("other");
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn resumes_at_the_next_pass_when_it_waits_on_nothing() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    nodes.run_commit("parent", || wait_on(Vec::new(), JoinPolicy::FailFast, 1));
    let parent = start(&root, &nodes, "parent").await;
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(nodes.log(), ["run:parent", "resume:parent"]);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_running_phases_after_spawning_and_waits_on_subsets_in_sequence() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    let found = Ids::default();
    let (sink, spawner) = (found.clone(), nodes.clone());
    nodes.run("parent", move |runtime, context| {
        let (sink, nodes) = (sink.clone(), spawner.clone());
        async move {
            let owner = runtime.task_id();
            let change = move |tx: Tx, _| async move {
                for name in ["a", "b", "c"] {
                    let id = spawn(&tx, &nodes, owner, name).await?;
                    sink.lock().unwrap().push(id);
                }
                Ok(resume_at(0))
            };
            runtime.commit(change, &context).await
        }
    });
    let (source, logger) = (found.clone(), nodes.clone());
    nodes.resume("parent", move |runtime, context, round| {
        logger.push(format!("round:{round}"));
        let found = ids(&source);
        let next = match round {
            0 => wait_on(vec![found[0]], JoinPolicy::AllSettled, 1),
            1 => wait_on(vec![found[1], found[2]], JoinPolicy::FailFast, 2),
            _ => end(Ending::Completed, "parent"),
        };
        async move { runtime.commit(move |_, _| async move { Ok(next) }, &context).await }
    });
    let parent = start(&root, &nodes, "parent").await;
    eventually(|| async { nodes.logged("round:0") }).await;
    until_status(&harness, parent, TaskStatus::Waiting).await;
    let found = ids(&found);
    nodes.open_ok("b");
    assert!(settled(wait_task(&harness, found[1])).await);
    assert_eq!(status(&harness, parent).await, TaskStatus::Waiting);
    nodes.open_ok("a");
    eventually(|| async { nodes.logged("round:1") }).await;
    nodes.open_ok("c");
    assert_eq!(outcome_of(&harness, parent).await, "completed");
    assert_eq!(nodes.lines("round:"), ["round:0", "round:1", "round:2"]);
    harness.close(&context()).await.unwrap();
}
