//! Left-running nested calls, key rules, nested calls of nested calls, and progress-free calls.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::*;
use serde_json::{Value, json};

use super::calls::{cancelled, listen, live_value};
use super::support::Deferred;
use super::support::harness_support::*;
use super::support::task_support::{aborted, eventually};
use super::{add_tool, args, call, done, echo_tool, nested_text, result_text, results, run_once, tool, tool_tasks};

fn keyed(key: &str) -> ExecuteToolOptions {
    ExecuteToolOptions { key: Some(key.into()), ..ExecuteToolOptions::default() }
}

/// Start `name` as a nested call of `api` and leave it running.
fn leave_running(api: &ToolExecutionApi, name: &'static str, context: &pi_chord::Context, options: ExecuteToolOptions) {
    let (caller, call_context) = (api.clone(), context.clone());
    tokio::spawn(async move { caller.execute_tool(name, Default::default(), &call_context, options).await });
}

#[tokio::test]
async fn aborts_left_running_nested_calls_of_a_caller_interrupted_while_it_cleaned_them_up() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    let setup = chat_setup(Default::default());
    let (running, cleaning, release) = (Deferred::default(), Deferred::default(), Deferred::default());
    let hang_runs = Arc::new(AtomicUsize::new(0));
    let (signals, count) = ((running.clone(), cleaning.clone(), release.clone()), hang_runs.clone());
    add_tool(
        &setup,
        tool("hang", move |_, api, context| {
            count.fetch_add(1, Ordering::SeqCst);
            let (running, cleaning, release) = signals.clone();
            async move {
                api.output("partial\n")?;
                api.details(Json::from(json!({ "step": 1 })), &context).await?;
                running.resolve();
                aborted(context.abort_signal().unwrap().clone()).await;
                // The caller's cleanup waits for this abort; hold it there until the Harness closes.
                cleaning.resolve();
                release.wait().await;
                Err(DurableError::error("aborted"))
            }
        }),
    );
    let gate = running.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let running = gate.clone();
            async move {
                leave_running(&api, "hang", &context, ExecuteToolOptions::default());
                running.wait().await;
                Ok(ToolExecutionResult::text("returned early"))
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    let id = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().id;
    cleaning.wait().await;
    let closer = harness.clone();
    let closed = tokio::spawn(async move { closer.close(&ctx()).await });
    super::support::flush().await;
    release.resolve();
    closed.await.unwrap().unwrap();

    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    harness.resume().unwrap();
    let settled = harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    harness.wait_for_idle(&ctx()).await.unwrap();
    // The caller is replay-unsafe: it reports the interruption, and its cleanup aborts the nested call.
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!((result.tool_call_id.as_str(), result.is_error), ("c1", true));
    assert!(result_text(&result).contains("Tool batch was interrupted"), "{}", result_text(&result));
    let child = tool_tasks(&harness).await.remove(1);
    assert_eq!(child.state.outcome().map(TaskOutcome::status), Some("aborted"));
    assert_eq!(hang_runs.load(Ordering::SeqCst), 1);
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_a_nested_call_whose_admission_was_still_committing_when_its_caller_returned() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    add_tool(&setup, tool("hang", |_, _, context| async move { cancelled(&context).await }));
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("echo", args(json!({ "text": "warm" })), &context, keyed("warm")).await?;
            leave_running(&api, "hang", &context, keyed("late"));
            // A spawned call starts when polled; let it reach its admission commit before returning.
            tokio::task::yield_now().await;
            Ok(ToolExecutionResult::default())
        }),
    );
    let (harness, root, status, _) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    let tasks = tool_tasks(&harness).await;
    let late = tasks
        .iter()
        .find(|task| task.input["kind"] == Json::from("nested") && task.input["call"]["name"] == Json::from("hang"))
        .unwrap();
    assert_eq!(late.state.outcome().map(TaskOutcome::status), Some("aborted"));
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn aborts_every_left_running_nested_call_before_waiting_for_any() {
    let setup = chat_setup(Default::default());
    let (b_aborted, started_a, started_b) = (Deferred::default(), Deferred::default(), Deferred::default());
    // A only stops once B was told to stop.
    let (gate, signal) = (b_aborted.clone(), started_a.clone());
    add_tool(
        &setup,
        tool("a", move |_, _, _| {
            let (b_aborted, started) = (gate.clone(), signal.clone());
            async move {
                started.resolve();
                b_aborted.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (gate, signal) = (b_aborted.clone(), started_b.clone());
    add_tool(
        &setup,
        tool("b", move |_, _, context| {
            let (b_aborted, started) = (gate.clone(), signal.clone());
            async move {
                started.resolve();
                aborted(context.abort_signal().unwrap().clone()).await;
                b_aborted.resolve();
                Err(DurableError::error("aborted"))
            }
        }),
    );
    let (a, b) = (started_a.clone(), started_b.clone());
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let (a, b) = (a.clone(), b.clone());
            async move {
                leave_running(&api, "a", &context, keyed("a"));
                leave_running(&api, "b", &context, keyed("b"));
                futures::join!(a.wait(), b.wait());
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, _, status, _) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    let nested: Vec<TaskStatus> = tool_tasks(&harness)
        .await
        .iter()
        .filter(|task| task.input["kind"] == Json::from("nested"))
        .map(|task| task.state.status())
        .collect();
    assert_eq!(nested, [TaskStatus::Terminal, TaskStatus::Terminal]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_explicit_keys_that_are_empty_have_a_slash_are_proto_or_are_positive_integers() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                for key in ["", "a/b", "__proto__", "7"] {
                    if let Err(error) = api.execute_tool("echo", Default::default(), &context, keyed(key)).await {
                        sink.lock().unwrap().push(error.to_string());
                    }
                }
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let rule = r#"must be non-empty, without "/", not "__proto__", and not a positive integer"#;
    let expected: Vec<String> =
        ["", "a/b", "__proto__", "7"].iter().map(|key| format!("Nested call key \"{key}\" {rule}")).collect();
    assert_eq!(*errors.lock().unwrap(), expected);
    harness.close(&ctx()).await.unwrap();
}

fn start_end_events(events: &[Value]) -> Vec<(String, String, Value)> {
    let tools =
        events.iter().filter(|event| event["type"] == "tool_execution_start" || event["type"] == "tool_execution_end");
    tools
        .map(|event| {
            let kind = event["type"].as_str().unwrap().to_owned();
            (kind, event["toolCallId"].as_str().unwrap().to_owned(), event["parentToolCallId"].clone())
        })
        .collect()
}

#[tokio::test]
async fn runs_nested_calls_of_nested_calls_and_ends_them_children_first() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    add_tool(
        &setup,
        tool("inner", |_, api, context| async move {
            let result = api.execute_tool("echo", args(json!({ "text": "deep" })), &context, keyed("x")).await?;
            Ok(ToolExecutionResult::text(nested_text(&result)))
        }),
    );
    add_tool(
        &setup,
        tool("outer", |_, api, context| async move {
            let result = api.execute_tool("inner", Default::default(), &context, ExecuteToolOptions::default()).await?;
            Ok(ToolExecutionResult::text(nested_text(&result)))
        }),
    );
    setup.faux.set_responses([call("outer"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, events) = listen(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    stream.stop().await;
    assert_eq!(result_text(&results(&all_entries(&root).await)[0]), "echo deep");
    let tasks = tool_tasks(&harness).await;
    assert_eq!((tasks[1].owner, tasks[2].owner), (Some(tasks[0].id), Some(tasks[1].id)));
    let expected = [
        ("tool_execution_start", "c1", Value::Null),
        ("tool_execution_start", "c1/1", json!("c1")),
        ("tool_execution_start", "c1/1/x", json!("c1/1")),
        ("tool_execution_end", "c1/1/x", json!("c1/1")),
        ("tool_execution_end", "c1/1", json!("c1")),
        ("tool_execution_end", "c1", Value::Null),
    ];
    let expected: Vec<(String, String, Value)> =
        expected.into_iter().map(|(a, b, c)| (a.into(), b.into(), c)).collect();
    assert_eq!(start_end_events(&events.lock().unwrap()), expected);
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_the_output_of_several_running_nested_calls_and_shows_them_to_a_late_subscriber() {
    let setup = chat_setup(Default::default());
    let release = Deferred::default();
    let (running_one, running_two, wrote_one, wrote_two) =
        (Deferred::default(), Deferred::default(), Deferred::default(), Deferred::default());
    let gates = (release.clone(), running_one.clone(), running_two.clone(), wrote_one.clone(), wrote_two.clone());
    add_tool(
        &setup,
        tool("slow", move |args, api, context| {
            let (release, running_one, running_two, wrote_one, wrote_two) = gates.clone();
            let name = args["text"].as_str().unwrap().to_owned();
            async move {
                let (running, wrote) = if name == "one" { (running_one, wrote_one) } else { (running_two, wrote_two) };
                running.resolve();
                wrote.wait().await;
                api.output(&format!("{name}\n"))?;
                api.details(Json::from(json!({ "name": name })), &context).await?;
                release.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            let (one, two) = futures::join!(
                api.execute_tool("slow", args(json!({ "text": "one" })), &context, ExecuteToolOptions::default()),
                api.execute_tool("slow", args(json!({ "text": "two" })), &context, ExecuteToolOptions::default())
            );
            one?;
            two?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    futures::join!(running_one.wait(), running_two.wait());
    let (stream, events) = listen(&harness, &root).await;
    let slots: Vec<(Value, Value)> = stream.snapshot["nestedTools"]
        .to_serde()
        .as_array()
        .unwrap()
        .iter()
        .map(|slot| (slot["callId"].clone(), slot["status"].clone()))
        .collect();
    assert_eq!(slots, [(json!("c1/1"), json!("running")), (json!("c1/2"), json!("running"))]);
    let updates = || -> Vec<(Value, Value)> {
        let events = events.lock().unwrap();
        let outputs =
            events.iter().filter(|event| event["type"] == "tool_execution_update" && event.get("output").is_some());
        outputs.map(|event| (event["toolCallId"].clone(), event["output"].clone())).collect()
    };
    // The second slot writes first, so a wrong slot index in the update path would show.
    wrote_two.resolve();
    eventually(|| async { updates().len() == 1 }).await;
    wrote_one.resolve();
    eventually(|| async { updates().len() == 2 }).await;
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    stream.stop().await;
    assert_eq!(updates(), [(json!("c1/2"), json!({ "set": "two\n" })), (json!("c1/1"), json!({ "set": "one\n" }))]);
    harness.close(&ctx()).await.unwrap();
}

fn chatty(running: &Deferred, release: Option<&Deferred>) -> ToolRegistration {
    let (running, release) = (running.clone(), release.cloned());
    tool("chatty", move |_, api, context| {
        let (running, release) = (running.clone(), release.clone());
        async move {
            api.output("line\n")?;
            api.diagnostic(ToolDiagnostic::new(DiagnosticSeverity::Info, "note"))?;
            api.details(Json::from(json!({ "step": 1 })), &context).await?;
            running.resolve();
            match release {
                Some(release) => {
                    release.wait().await;
                    Ok(ToolExecutionResult::default())
                }
                None => cancelled(&context).await,
            }
        }
    })
}

fn quiet() -> ExecuteToolOptions {
    ExecuteToolOptions { progress: Some(false), ..ExecuteToolOptions::default() }
}

#[tokio::test]
async fn commits_only_the_status_of_a_nested_call_made_with_progress_false_and_keeps_its_output_in_the_result() {
    let setup = chat_setup(Default::default());
    let (running, release) = (Deferred::default(), Deferred::default());
    add_tool(&setup, chatty(&running, Some(&release)));
    let result = Arc::new(Mutex::new(None));
    let sink = result.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                *sink.lock().unwrap() = Some(api.execute_tool("chatty", Default::default(), &context, quiet()).await?);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    // `details()` resolved before this, so a progress commit would have landed already.
    running.wait().await;
    let mut slot = live_value(&harness, &root).await["nestedTools"][0].clone();
    let fields = slot.as_object_mut().unwrap();
    assert!(fields.remove("parentTaskId").unwrap().is_number() && fields.remove("taskId").unwrap().is_number());
    let expected =
        json!({ "callId": "c1/1", "parentCallId": "c1", "name": "chatty", "arguments": {}, "status": "running" });
    assert_eq!(slot, expected);
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    let result = result.lock().unwrap().clone().unwrap();
    assert_eq!(nested_text(&result), "line\n");
    assert_eq!(result.details.map(|details| details.to_serde()), Some(json!({ "step": 1 })));
    let diagnostics = serde_json::to_value(&result.diagnostics).unwrap();
    assert_eq!(diagnostics, json!([{ "severity": "info", "message": "note" }]));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn loses_the_running_output_of_a_nested_call_made_with_progress_false_when_it_is_aborted() {
    let setup = chat_setup(Default::default());
    let running = Deferred::default();
    add_tool(&setup, chatty(&running, None));
    let gate = running.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let running = gate.clone();
            async move {
                leave_running(&api, "chatty", &context, quiet());
                running.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let (stream, events) = listen(&harness, &root).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    stream.stop().await;
    // The abort's result is built from the slot, which never got the output.
    let events = events.lock().unwrap().clone();
    let end =
        events.iter().find(|event| event["type"] == "tool_execution_end" && event["toolCallId"] == "c1/1").unwrap();
    let result = &end["result"];
    assert_eq!(result["isError"], true);
    assert!(result.get("structuredOutput").is_none() && result.get("details").is_none(), "{result}");
    let codes: Vec<Value> =
        result["diagnostics"].as_array().unwrap().iter().map(|diagnostic| diagnostic["code"].clone()).collect();
    assert_eq!(codes, [json!("aborted")]);
    harness.close(&ctx()).await.unwrap();
}
