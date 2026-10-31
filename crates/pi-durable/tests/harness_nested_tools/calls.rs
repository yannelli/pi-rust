//! Nested call keys, usage, slots, aborts with the caller, and reattachment cases.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::{AbortController, ChordError, Context, Json, with_abort_signal};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::usage::USAGE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::*;
use serde_json::{Value, json};

use super::support::Deferred;
use super::support::harness_support::*;
use super::support::live_support::{ended_at, nested_result_of, nested_results, slots_of, start_order, view_history};
use super::support::task_support::aborted;
use super::{add_tool, args, call, done, echo_tool, nested_text, result_text, results, run_once, tool, tool_tasks};

pub async fn cancelled(context: &Context) -> DurableResult<ToolExecutionResult> {
    let signal = context.abort_signal().unwrap().clone();
    aborted(signal.clone()).await;
    Err(signal.reason().map_or_else(DurableError::aborted, DurableError::from))
}

pub async fn live_value(harness: &Harness, conversation: &Conversation) -> Value {
    harness.snapshot(&LIVE_DOC, conversation.id, &ctx()).await.unwrap().map(|live| live.to_serde()).unwrap_or_default()
}

#[tokio::test]
async fn matches_a_reused_key_s_arguments_regardless_of_key_order() {
    let setup = chat_setup(Default::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let count = runs.clone();
    let parameters = json!({
        "type": "object",
        "properties": { "a": { "type": "string" }, "b": { "type": "string" } },
        "required": ["a", "b"]
    });
    let pair = define_tool("pair", "pair", parameters, move |args, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        let text = format!("{}{}", args["a"].as_str().unwrap(), args["b"].as_str().unwrap());
        async move { Ok(ToolExecutionResult::text(text)) }
    });
    add_tool(&setup, pair);
    let nested = Arc::new(Mutex::new(Vec::new()));
    let sink = nested.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                let keyed = || ExecuteToolOptions { key: Some("k".into()), ..ExecuteToolOptions::default() };
                let first = api.execute_tool("pair", args(json!({ "a": "x", "b": "y" })), &context, keyed()).await?;
                let second = api.execute_tool("pair", args(json!({ "b": "y", "a": "x" })), &context, keyed()).await?;
                sink.lock().unwrap().extend([nested_text(&first), nested_text(&second)]);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(*nested.lock().unwrap(), ["xy", "xy"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_the_nested_calls_a_caller_left_running_when_it_returns_keeping_their_details() {
    let setup = chat_setup(Default::default());
    let running = Deferred::default();
    let signal = running.clone();
    add_tool(
        &setup,
        tool("hang", move |_, api, context| {
            let running = signal.clone();
            async move {
                api.output("partial\n")?;
                api.details(Json::from(json!({ "step": 1 })), &context).await?;
                running.resolve();
                cancelled(&context).await
            }
        }),
    );
    let gate = running.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let running = gate.clone();
            async move {
                let caller = api.clone();
                let call_context = context.clone();
                let _abandoned = tokio::spawn(async move {
                    caller.execute_tool("hang", Default::default(), &call_context, ExecuteToolOptions::default()).await
                });
                running.wait().await;
                Ok(ToolExecutionResult::text("returned early"))
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    let nested = nested_results(&harness);
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    harness.wait_for_idle(&ctx()).await.unwrap();
    history.detach();
    assert_eq!(result_text(&results(&all_entries(&root).await)[0]), "returned early");
    let child = tool_tasks(&harness).await.remove(1);
    let state = serde_json::to_value(&child.state).unwrap();
    assert_eq!((state["status"].as_str(), state["outcome"]["status"].as_str()), (Some("terminal"), Some("aborted")));
    assert_eq!(state["outcome"]["result"]["kind"], "nested");
    // The aborted call's result keeps its details; programs get no partial output, which only the model read.
    let child_result = nested_result_of(&history.live(), &nested, "c1/1").unwrap();
    assert_eq!((child_result["isError"].clone(), child_result["details"].clone()), (json!(true), json!({ "step": 1 })));
    assert!(child_result.get("structuredOutput").is_none());
    // The nested call ends before its caller.
    let live = history.live();
    let (child_end, caller_end) = (ended_at(&live, "c1/1").unwrap(), ended_at(&live, "c1").unwrap());
    assert!(child_end > 0 && child_end < caller_end, "{child_end} {caller_end}");
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_a_nested_call_s_usage_under_its_tool() {
    let setup = chat_setup(Default::default());
    let usage = json!({
        "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3,
        "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3 }
    });
    let typed: pi_ai::Usage = serde_json::from_value(usage.clone()).unwrap();
    add_tool(
        &setup,
        tool(
            "paid",
            move |_, _, _| async move { Ok(ToolExecutionResult { usage: Some(typed), ..Default::default() }) },
        ),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("paid", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    let (harness, root, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let stored = harness.snapshot(&USAGE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    let expected: Value = serde_json::from_str(&serde_json::to_string(&json!({ "paid": usage })).unwrap()).unwrap();
    assert_eq!(Json::from(stored["tools"].to_serde()), Json::from(expected));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_nested_calls_in_pi_live_nested_tools_with_their_parent() {
    let setup = chat_setup(Default::default());
    let (release, running) = (Deferred::default(), Deferred::default());
    let (gate, signal) = (release.clone(), running.clone());
    add_tool(
        &setup,
        tool("slow", move |_, api, context| {
            let (release, running) = (gate.clone(), signal.clone());
            async move {
                api.output("working\n")?;
                api.details(Json::from(json!({ "step": 1 })), &context).await?;
                running.resolve();
                release.wait().await;
                Ok(ToolExecutionResult::text("slow done"))
            }
        }),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            let result = api.execute_tool("slow", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::text(nested_text(&result)))
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let history = view_history(&harness, root.id).await;
    assert!(history.live()[0].get("nestedTools").is_none());
    let nested = nested_results(&harness);
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    running.wait().await;
    let mut slot = live_value(&harness, &root).await["nestedTools"][0].clone();
    let slot_object = slot.as_object_mut().unwrap();
    assert!(
        slot_object.remove("parentTaskId").unwrap().is_number() && slot_object.remove("taskId").unwrap().is_number()
    );
    let expected = json!({
        "callId": "c1/1", "parentCallId": "c1", "name": "slow", "arguments": {}, "status": "running",
        "output": "working\n", "details": { "step": 1 }
    });
    assert_eq!(slot, expected);
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    history.detach();
    let states = history.live();
    assert_eq!(start_order(&states), ["c1", "c1/1"]);
    assert!(ended_at(&states, "c1/1").unwrap() < ended_at(&states, "c1").unwrap());
    let result = nested_result_of(&states, &nested, "c1/1").unwrap();
    assert_eq!((&result["structuredOutput"], &result["isError"]), (&json!("slow done"), &json!(false)));
    // Slots name the call's task and, for a nested call, the calling one.
    let tasks = tool_tasks(&harness).await;
    let (parent, child) = (json!(tasks[0].id), json!(tasks[1].id));
    let nested_slot = slots_of(&states, "c1/1").into_iter().flatten().next().unwrap();
    assert_eq!((&nested_slot["taskId"], &nested_slot["parentCallId"]), (&child, &json!("c1")));
    assert_eq!(nested_slot["parentTaskId"], parent);
    let caller = slots_of(&states, "c1").into_iter().flatten().find(|slot| slot["taskId"].is_u64()).unwrap();
    assert_eq!(caller["taskId"], parent);
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

fn hang(running: &Deferred) -> ToolRegistration {
    let signal = running.clone();
    tool("hang", move |_, _, context| {
        let running = signal.clone();
        async move {
            running.resolve();
            cancelled(&context).await
        }
    })
}

#[tokio::test]
async fn aborts_nested_calls_with_their_caller() {
    let setup = chat_setup(Default::default());
    let running = Deferred::default();
    add_tool(&setup, hang(&running));
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("hang", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    running.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    assert_eq!(submission.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    let tasks = tool_tasks(&harness).await;
    let outcome = |record: &TaskRecord| serde_json::to_value(record.state.outcome()).unwrap();
    let (parent, child) = (outcome(&tasks[0]), outcome(&tasks[1]));
    assert_eq!((child["status"].clone(), child["result"]["kind"].clone()), (json!("aborted"), json!("nested")));
    assert_eq!((parent["status"].clone(), parent["result"]["kind"].clone()), (json!("aborted"), json!("model")));
    let live = live_value(&harness, &root).await;
    assert_eq!(live.as_object().unwrap().keys().collect::<Vec<_>>(), ["runFailure"]);
    assert_eq!(live["runFailure"]["reason"], json!("aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_running_nested_call_when_its_caller_throws() {
    let setup = chat_setup(Default::default());
    let running = Deferred::default();
    add_tool(&setup, hang(&running));
    let gate = running.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let running = gate.clone();
            async move {
                let (caller, call_context) = (api.clone(), context.clone());
                tokio::spawn(async move {
                    caller.execute_tool("hang", Default::default(), &call_context, ExecuteToolOptions::default()).await
                });
                running.wait().await;
                Err(DurableError::error("caller failed"))
            }
        }),
    );
    let (harness, _, status, entries) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    assert!(result_text(&results(&entries)[0]).contains("caller failed"));
    let child = tool_tasks(&harness).await.remove(1);
    assert_eq!(child.state.outcome().map(TaskOutcome::status), Some("aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reattaches_a_replay_safe_caller_to_the_nested_calls_it_made_before_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = chat_setup(Default::default());
    let echo_runs = Arc::new(AtomicUsize::new(0));
    add_tool(&setup, echo_tool(echo_runs.clone()));
    let (blocked, runs) = (Deferred::default(), Arc::new(AtomicUsize::new(0)));
    let (signal, count) = (blocked.clone(), runs.clone());
    let batch = tool("batch", move |_, api, context| {
        let (blocked, run) = (signal.clone(), count.fetch_add(1, Ordering::SeqCst) + 1);
        async move {
            let echo = |text: &str| args(json!({ "text": text }));
            let first = api.execute_tool("echo", echo("a"), &context, ExecuteToolOptions::default()).await?;
            if run == 1 {
                blocked.resolve();
                cancelled(&context).await?;
            }
            // A call new to the rerun: its slot, after the reattached one's, is found and finishes.
            let second = api.execute_tool("echo", echo("b"), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::text(format!("run {run}: {}, {}", nested_text(&first), nested_text(&second))))
        }
    });
    add_tool(&setup, batch.with_replay(ToolReplay::Safe));
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    blocked.wait().await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    harness.resume().unwrap();
    let settled = harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!((runs.load(Ordering::SeqCst), echo_runs.load(Ordering::SeqCst)), (2, 2));
    assert_eq!(result_text(&results(&all_entries(&root).await)[0]), "run 2: echo a, echo b");
    let tasks = tool_tasks(&harness).await;
    assert_eq!(tasks.iter().filter(|task| task.input["kind"] == Json::from("nested")).count(), 2);
    let outcome = serde_json::to_value(tasks[1].state.outcome()).unwrap();
    assert_eq!(outcome["result"]["kind"], "nested");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn stops_only_the_wait_when_a_caller_cancels_it_the_call_runs_on_and_its_key_returns_its_result() {
    let setup = chat_setup(Default::default());
    let cancel = AbortController::new();
    let release = Deferred::default();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let retry = Arc::new(Mutex::new(None));
    let (aborter, gate) = (cancel.clone(), release.clone());
    add_tool(
        &setup,
        tool("slow", move |_, _, _| {
            aborter.abort(Some(ChordError::error("cancelled")));
            let release = gate.clone();
            async move {
                release.wait().await;
                Ok(ToolExecutionResult::text("slow done"))
            }
        }),
    );
    let (signal, gate, sink, retried) = (cancel.signal(), release.clone(), attempts.clone(), retry.clone());
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let (cancelled, release, sink, retried) =
                (with_abort_signal(&signal, &context), gate.clone(), sink.clone(), retried.clone());
            async move {
                let keyed = || ExecuteToolOptions { key: Some("same".into()), ..ExecuteToolOptions::default() };
                let first = api.execute_tool("slow", Default::default(), &cancelled, keyed()).await;
                sink.lock().unwrap().push(if first.is_ok() { "first done" } else { "first cancelled" });
                release.resolve();
                *retried.lock().unwrap() = Some(api.execute_tool("slow", Default::default(), &context, keyed()).await?);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(*attempts.lock().unwrap(), ["first cancelled"]);
    let retry = retry.lock().unwrap().clone().unwrap();
    assert_eq!((retry.is_error, nested_text(&retry)), (false, "slow done".to_owned()));
    assert_eq!(tool_tasks(&harness).await.len(), 2);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn admits_one_nested_call_for_a_key_used_by_concurrent_calls() {
    let setup = chat_setup(Default::default());
    let runs = Arc::new(AtomicUsize::new(0));
    add_tool(&setup, echo_tool(runs.clone()));
    let both = Arc::new(Mutex::new(Vec::new()));
    let sink = both.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                let keyed = || ExecuteToolOptions { key: Some("k".into()), ..ExecuteToolOptions::default() };
                let echo = || args(json!({ "text": "a" }));
                let (first, second) = futures::join!(
                    api.execute_tool("echo", echo(), &context, keyed()),
                    api.execute_tool("echo", echo(), &context, keyed())
                );
                sink.lock().unwrap().extend([nested_text(&first?), nested_text(&second?)]);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(*both.lock().unwrap(), ["echo a", "echo a"]);
    assert_eq!(tool_tasks(&harness).await.len(), 2);
    harness.close(&ctx()).await.unwrap();
}
