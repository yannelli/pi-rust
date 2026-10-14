//! Replay-safe callers and child tasks across a restart.

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::Deferred;
use super::support::chat::{add_tool, answer, call};
use super::support::harness_support::*;
use super::{cancelled, codes, nested_tasks, open, outcome_status, results, settle, sqlite_path, submit, tool, worker};

#[tokio::test]
async fn lets_a_replay_safe_caller_s_unfinished_nested_calls_recover_and_reattaches_to_them_by_key() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    // Runs of `safe`, `unsafe`, and the caller.
    let runs = Arc::new(Mutex::new((0, 0, 0)));
    let (safe_running, unsafe_running) = (Deferred::default(), Deferred::default());
    let (count, signal) = (runs.clone(), safe_running.clone());
    let safe = tool("safe", move |_, _, context| {
        let (count, running) = (count.clone(), signal.clone());
        async move {
            let run = {
                let mut runs = count.lock().unwrap();
                runs.0 += 1;
                runs.0
            };
            if run == 1 {
                running.resolve();
                cancelled(&context).await?;
            }
            Ok(ToolExecutionResult::text(format!("safe run {run}")))
        }
    });
    add_tool(&setup, safe.with_replay(ToolReplay::Safe));
    let (count, signal) = (runs.clone(), unsafe_running.clone());
    add_tool(
        &setup,
        tool("unsafe", move |_, _, context| {
            let (count, running) = (count.clone(), signal.clone());
            async move {
                count.lock().unwrap().1 += 1;
                running.resolve();
                cancelled(&context).await
            }
        }),
    );
    let received = Arc::new(Mutex::new(Vec::new()));
    let (count, sink) = (runs.clone(), received.clone());
    let batch = tool("batch", move |_, api, context| {
        let (count, sink) = (count.clone(), sink.clone());
        async move {
            count.lock().unwrap().2 += 1;
            let first = api.execute_tool("safe", Default::default(), &context, ExecuteToolOptions::default());
            let second = api.execute_tool("unsafe", Default::default(), &context, ExecuteToolOptions::default());
            let (first, second) = futures::join!(first, second);
            *sink.lock().unwrap() = vec![first?, second?];
            Ok(ToolExecutionResult::default())
        }
    });
    add_tool(&setup, batch.with_replay(ToolReplay::Safe));
    setup.faux.set_responses([call("batch", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    safe_running.wait().await;
    unsafe_running.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(*runs.lock().unwrap(), (2, 1, 2));
    // Default keys follow call order, so the rerun found both calls.
    let tasks = nested_tasks(&harness).await;
    assert_eq!(tasks.len(), 2);
    for task in &tasks {
        assert_eq!((task.abandon_on_restart, task.abort_reason), (None, None));
    }
    let received = received.lock().unwrap().clone();
    assert_eq!(received[0].structured_output, Some(Json::from("safe run 2")));
    assert_eq!(codes(received.get(1)), ["interrupted"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reattaches_a_replay_safe_caller_by_default_and_explicit_keys_and_rejects_a_rerun_that_calls_differently() {
    for order in ["same", "swapped"] {
        let directory = tempfile::tempdir().unwrap();
        let path = sqlite_path(&directory);
        let setup = chat_setup(Default::default());
        let runs = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = runs.clone();
        add_tool(
            &setup,
            tool("echo", move |args, _, _| {
                let text = args.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
                sink.lock().unwrap().push(text.clone());
                async move { Ok(ToolExecutionResult::text(format!("echo {text}"))) }
            }),
        );
        let blocked = Deferred::default();
        let caller_runs = Arc::new(Mutex::new(0));
        let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
        let (signal, count, sink) = (blocked.clone(), caller_runs.clone(), seen.clone());
        let batch = tool("batch", move |_, api, context| {
            let (blocked, sink) = (signal.clone(), sink.clone());
            let run = {
                let mut runs = count.lock().unwrap();
                *runs += 1;
                *runs
            };
            async move {
                let first = if run == 2 && order == "swapped" { "b" } else { "a" };
                let args = |text: &str| json!({ "text": text }).as_object().cloned().unwrap();
                let one = api.execute_tool("echo", args(first), &context, ExecuteToolOptions::default()).await?;
                let keyed = ExecuteToolOptions { key: Some("named".into()), ..ExecuteToolOptions::default() };
                let named = api.execute_tool("echo", args("named"), &context, keyed).await?;
                if run == 1 {
                    blocked.resolve();
                    cancelled(&context).await?;
                }
                *sink.lock().unwrap() =
                    [one, named].into_iter().filter_map(|result| result.structured_output).collect();
                Ok(ToolExecutionResult::default())
            }
        });
        add_tool(&setup, batch.with_replay(ToolReplay::Safe));
        setup.faux.set_responses([call("batch", "c1"), answer("done")]);
        let (harness, root) = open(&path, &setup).await;
        let id = submit(&root).await;
        blocked.wait().await;
        harness.close(&ctx()).await.unwrap();

        let (harness, root) = open(&path, &setup).await;
        harness.resume().unwrap();
        assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
        let result = results(&all_entries(&root).await).remove(0);
        assert_eq!(*runs.lock().unwrap(), ["a", "named"]);
        if order == "same" {
            assert_eq!(*seen.lock().unwrap(), [Json::from("echo a"), Json::from("echo named")]);
            assert!(!result.is_error);
        } else {
            // Key 1 was made with other arguments: the rerun throws instead of reusing a different call's result.
            assert!(result.is_error);
            let content = serde_json::to_string(&result.content).unwrap();
            assert!(content.contains("Nested call c1/1 was already made with another tool"), "{content}");
        }
        harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn abandons_the_child_tasks_a_replay_unsafe_tool_created_and_awaited_in_memory() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (log, started) = (Arc::new(Mutex::new(Vec::new())), Deferred::default());
    let task = worker("test.direct-worker", &log, &started);
    setup.registry.install(Extension::new("direct-worker").task(task.clone())).unwrap();
    add_tool(
        &setup,
        tool("work", move |_, api, context| {
            let task = task.clone();
            async move {
                let id = api.create_task(task, Json::Null, TaskOptions::owned_by(api.task_id), &context).await?;
                api.wait_for_task(id, &context).await?;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("work", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    started.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    assert_eq!(*log.lock().unwrap(), ["work", "abort restart"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn leaves_background_work_under_an_abandoned_nested_call_running_and_settles_without_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let (log, started) = (Arc::new(Mutex::new(Vec::new())), Deferred::default());
    let task = worker("test.background", &log, &started);
    setup.registry.install(Extension::new("background").task(task.clone())).unwrap();
    let background = Arc::new(Mutex::new(None));
    let sink = background.clone();
    add_tool(
        &setup,
        tool("spawn", move |_, api, context| {
            let (task, sink) = (task.clone(), sink.clone());
            async move {
                // A conversation the call owns, with background work in it, as a persistent subagent has.
                let owner = api.task_id;
                let spawn = move |tx: Tx| async move {
                    let child = tx.create_conversation(ConversationOwnership::Task { task_id: owner }).await?;
                    let options = TaskOptions { background: true, ..TaskOptions::conversation(child.id) };
                    tx.create_task(task, Json::Null, options).await
                };
                *sink.lock().unwrap() = Some(api.commit(spawn, &context).await?);
                cancelled(&context).await
            }
        }),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("spawn", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch", "c1"), answer("done")]);
    let (harness, root) = open(&path, &setup).await;
    let id = submit(&root).await;
    started.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &setup).await;
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await, SubmissionStatus::Done);
    let spawn = nested_tasks(&harness).await.remove(0);
    assert_eq!(spawn.abort_reason, Some(AbortReason::Restart));
    assert!(outcome_status(&spawn).is_some());
    let background = background.lock().unwrap().unwrap();
    let record = harness.get_task(background, &ctx()).await.unwrap().unwrap();
    assert!(!record.abort_requested);
    assert_eq!(record.state.status(), TaskStatus::Running);
    harness.abort_task(background, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
}
