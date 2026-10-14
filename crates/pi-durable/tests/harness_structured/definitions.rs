//! Upstream `describe("definitions and waits")`.

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::json;

use super::nodes::*;
use super::support::context;

/// A waiter of its own kind, so its definition can be removed or replaced.
fn versioned(version: u32) -> Task {
    let definition = TaskDefinition::new("test.versioned", version, |_| Ok(Json::from(json!({ "phase": "wait" }))))
        .phase("wait", |task, runtime, context| async move {
            let on: Vec<TaskId> = serde_json::from_value(task.input["on"].to_serde()).unwrap();
            let checkpoint = Json::from(json!({ "phase": "resume" }));
            let state = TaskState::Waiting { checkpoint, on, policy: JoinPolicy::AllSettled };
            runtime.commit(move |_, _| async move { Ok(Some(state)) }, &context).await
        })
        .phase("resume", |_, runtime, context| async move {
            runtime.commit(|_, _| async { Ok(completed_with("v1")) }, &context).await
        })
        .phase("migrated", |_, runtime, context| async move {
            runtime.commit(|_, _| async { Ok(completed_with("v2")) }, &context).await
        })
        .abort(
            |_, runtime, context| async move { runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await },
        );
    define_task(if version == 2 {
        definition.migrate(|input, _, _| Ok((input.clone(), Json::from(json!({ "phase": "migrated" })))))
    } else {
        definition
    })
}

fn completed_with(result: &str) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(Json::from(result)) } })
}

fn add_task(registry: &Registry, task: Task) {
    registry.install(Extension::new(format!("task:{}", task.name)).task(task)).unwrap();
}

async fn create_waiter(root: &Conversation, other: TaskId) -> TaskId {
    let id = root.id;
    let create = move |tx: Tx| async move {
        tx.create_task(versioned(1), Json::from(json!({ "on": [other] })), TaskOptions::conversation(id)).await
    };
    root.commit(create, &context()).await.unwrap()
}

#[tokio::test]
async fn keeps_a_waiting_task_blocked_without_its_definition_and_resumes_it_migrated_under_a_newer_one() {
    let Opened { harness, root, registry, nodes, .. } = open_nodes().await;
    add_task(&registry, versioned(1));
    let other = start(&root, &nodes, "other").await;
    let waiter = create_waiter(&root, other).await;
    until_status(&harness, waiter, TaskStatus::Waiting).await;
    registry.uninstall("task:test.versioned").unwrap();
    nodes.open_ok("other");
    harness.wait_for_task(other, &context()).await.unwrap();
    let inspection = harness.inspect(&context()).await.unwrap();
    let inspected = inspection.tasks.into_iter().find(|task| task.record.id == waiter).map(|task| task.state);
    let blocked = TaskInspectionState::Blocked { reason: BlockedReason::MissingTask, error: None };
    assert_eq!(inspected, Some(blocked));
    assert_eq!(status(&harness, waiter).await, TaskStatus::Waiting);
    add_task(&registry, versioned(2));
    assert_eq!(outcome_json(&harness, waiter).await, json!({ "status": "completed", "result": "v2" }));
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn orphans_an_aborted_waiting_task_without_its_definition_at_once_leaving_the_task_it_waits_on_running() {
    let Opened { harness, root, registry, nodes, .. } = open_nodes().await;
    add_task(&registry, versioned(1));
    let other = start(&root, &nodes, "other").await;
    let waiter = create_waiter(&root, other).await;
    until_status(&harness, waiter, TaskStatus::Waiting).await;
    registry.uninstall("task:test.versioned").unwrap();
    assert_eq!(harness.abort_task(waiter, &context()).await.unwrap(), AbortTaskResult::Marked);
    let outcome = serde_json::to_value(state(&harness, waiter).await.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "orphaned", "reason": "missing_task" }));
    assert_eq!(status(&harness, other).await, TaskStatus::Running);
    nodes.open_ok("other");
    harness.close(&context()).await.unwrap();
}

fn holder(nodes: &Nodes, version: u32) -> Task {
    let spawner = nodes.clone();
    define_task(
        TaskDefinition::new("test.holder", version, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", move |task, runtime, context| {
                let nodes = spawner.clone();
                async move {
                    let owner = task.id;
                    let change = move |tx: Tx, _| async move {
                        spawn(&tx, &nodes, owner, "child").await?;
                        Ok(None)
                    };
                    runtime.commit(change, &context).await?;
                    runtime.commit(|_, _| async { Ok(completed_with("held")) }, &context).await
                }
            })
            .abort(|_, runtime, context| async move { runtime.commit(|_, _| async { Ok(aborted_state()) }, &context).await })
            .migrate(|_, _, _| Err(DurableError::error("never migrates"))),
    )
}

#[tokio::test]
async fn never_migrates_a_held_outcome_a_newer_definition_leaves_it_and_its_version_alone() {
    let Opened { harness, root, registry, reports, nodes } = open_nodes().await;
    add_task(&registry, holder(&nodes, 1));
    let (task, id) = (holder(&nodes, 1), root.id);
    let create = move |tx: Tx| async move { tx.create_task(task, Json::Null, TaskOptions::conversation(id)).await };
    let parent = root.commit(create, &context()).await.unwrap();
    until_status(&harness, parent, TaskStatus::Completing).await;
    // The same extension name replaces the old one in place.
    add_task(&registry, holder(&nodes, 2));
    nodes.open_ok("child");
    let settled_parent = harness.wait_for_task(parent, &context()).await.unwrap();
    let outcome = serde_json::to_value(settled_parent.state.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "completed", "result": "held" }));
    assert_eq!(settled_parent.version, 1);
    assert!(reports.lock().unwrap().is_empty());
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn treats_a_held_task_as_live_outcomes_rejects_and_a_task_waiting_on_it_resumes_at_its_final_commit() {
    let Opened { harness, root, nodes, .. } = open_nodes().await;
    spawn_child(&nodes, "held", "child", false);
    let held = start(&root, &nodes, "held").await;
    until_status(&harness, held, TaskStatus::Completing).await;
    let rejection = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = rejection.clone();
    nodes.run("reader", move |runtime, context| {
        let sink = sink.clone();
        async move {
            let read = runtime.outcomes(&[held], &context).await;
            *sink.lock().unwrap() = read.map_or_else(|error| error.to_string(), |_| "resolved".into());
            runtime
                .commit(move |_, _| async move { Ok(wait_on(vec![held], JoinPolicy::AllSettled, 1)) }, &context)
                .await
        }
    });
    let reader = start(&root, &nodes, "reader").await;
    until_status(&harness, reader, TaskStatus::Waiting).await;
    assert_eq!(*rejection.lock().unwrap(), format!("Task {held} is not terminal"));
    nodes.open_ok("child");
    assert_eq!(outcome_of(&harness, reader).await, "completed");
    assert!(nodes.logged("resume:reader"));
    harness.close(&context()).await.unwrap();
}
