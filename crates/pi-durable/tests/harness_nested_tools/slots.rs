//! Nested call slots, error results, and version 1 tool task records.

use std::sync::{Arc, Mutex};

use pi_ai::ContentBlock;
use pi_chord::Json;
use pi_durable::entries::ASSISTANT_ENTRY;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{BeforeToolDecision, ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;
use serde_json::{Value, json};

use super::calls::live_value;
use super::support::Deferred;
use super::support::harness_support::*;
use super::{add_tool, args, call, done, echo_tool, nested_text, result_text, results, run_once, tool, tool_tasks};

fn plain() -> ExecuteToolOptions {
    ExecuteToolOptions::default()
}

#[tokio::test]
async fn shows_a_nested_call_s_arguments_in_its_slot_as_made_then_unbounded_as_it_runs_with_them() {
    let setup = chat_setup(Default::default());
    let (hook_entered, hook_release, running, release) =
        (Deferred::default(), Deferred::default(), Deferred::default(), Deferred::default());
    let large = "x".repeat(64 * 1024);
    let (signal, gate) = (running.clone(), release.clone());
    add_tool(
        &setup,
        tool("slow", move |_, _, _| {
            let (running, release) = (signal.clone(), gate.clone());
            async move {
                running.resolve();
                release.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (entered, gate, text) = (hook_entered.clone(), hook_release.clone(), large.clone());
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(move |call: ToolHookCall, _, _| {
            let (entered, release, text) = (entered.clone(), gate.clone(), text.clone());
            Box::pin(async move {
                if call.parent.is_none() {
                    return Ok(None);
                }
                entered.resolve();
                release.wait().await;
                Ok(Some(BeforeToolDecision { arguments: Some(args(json!({ "text": text }))), ..Default::default() }))
            })
        })),
        ..ToolHooks::default()
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            api.execute_tool("slow", args(json!({ "text": "asked" })), &context, plain()).await?;
            Ok(ToolExecutionResult::default())
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    let slot = || async { live_value(&harness, &root).await["nestedTools"][0].clone() };
    hook_entered.wait().await;
    let pending = slot().await;
    assert_eq!(
        (pending["status"].clone(), pending["arguments"].clone()),
        (json!("pending"), json!({ "text": "asked" }))
    );
    hook_release.resolve();
    running.wait().await;
    let running_slot = slot().await;
    assert_eq!(
        (running_slot["status"].clone(), running_slot["arguments"].clone()),
        (json!("running"), json!({ "text": large }))
    );
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    harness.wait_for_idle(&ctx()).await.unwrap();
    // The slot showed the arguments the call runs with; the task input keeps the ones it was made with.
    let tasks = tool_tasks(&harness).await;
    let nested = tasks.iter().find(|task| task.input["kind"] == Json::from("nested")).unwrap();
    assert_eq!(nested.input["call"]["arguments"].to_serde(), json!({ "text": "asked" }));
    harness.close(&ctx()).await.unwrap();
}

// Upstream repairs to `{ text: undefined }`, which JSON drops; a Rust `Value` has no undefined, so the repair
// returns `{}`.
#[tokio::test]
async fn runs_a_nested_call_whose_repaired_arguments_set_an_optional_property_to_undefined() {
    let setup = chat_setup(Default::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let mut repaired = tool("repaired", move |args, _, _| {
        sink.lock().unwrap().push(Value::Object(args));
        async { Ok(ToolExecutionResult::default()) }
    });
    repaired.prepare_arguments = Some(Arc::new(|_: &Value| Ok(json!({}))));
    add_tool(&setup, repaired);
    let result = Arc::new(Mutex::new(None));
    let sink = result.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                *sink.lock().unwrap() =
                    Some(api.execute_tool("repaired", args(json!({ "text": "x" })), &context, plain()).await?);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    assert!(!result.lock().unwrap().as_ref().unwrap().is_error);
    assert_eq!(*seen.lock().unwrap(), [json!({})]);
    harness.close(&ctx()).await.unwrap();
}

fn summaries(live: &Value) -> Vec<Value> {
    let slots = live["nestedTools"].as_array().cloned().unwrap_or_default();
    slots
        .into_iter()
        .map(|slot| {
            let mut summary = slot["summary"].clone();
            // Durations vary; keep only that one was recorded.
            if let Some(fields) = summary.as_object_mut()
                && let Some(duration) = fields.remove("durationMs")
            {
                assert!(duration.is_number());
                fields.insert("durationMs".into(), json!("any"));
            }
            json!({ "callId": slot["callId"], "status": slot["status"], "summary": summary })
        })
        .collect()
}

#[tokio::test]
async fn finds_each_nested_call_s_slot_after_slots_in_the_middle_were_removed() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, echo_tool(Arc::default()));
    add_tool(
        &setup,
        tool("inner", |_, api, context| async move {
            for text in ["m1", "m2"] {
                api.execute_tool("echo", args(json!({ "text": text })), &context, plain()).await?;
            }
            Ok(ToolExecutionResult::default())
        }),
    );
    let (before, checked) = (Deferred::default(), Deferred::default());
    let slots = Arc::new(Mutex::new(Value::Null));
    let (signal, gate, sink) = (before.clone(), checked.clone(), slots.clone());
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let (before, checked, sink) = (signal.clone(), gate.clone(), sink.clone());
            async move {
                api.execute_tool("echo", args(json!({ "text": "n1" })), &context, plain()).await?;
                // n2 makes m1 and m2 and settles, which removes them from between n2 and n3.
                api.execute_tool("inner", Default::default(), &context, plain()).await?;
                for text in ["n3", "n4"] {
                    api.execute_tool("echo", args(json!({ "text": text })), &context, plain()).await?;
                }
                let live = api.snapshot(&LIVE_DOC, api.conversation_id, &context).await?.unwrap();
                *sink.lock().unwrap() = live.to_serde();
                before.resolve();
                checked.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    before.wait().await;
    let done_slot =
        |id: &str| json!({ "callId": id, "status": "done", "summary": { "isError": false, "durationMs": "any" } });
    let expected: Vec<Value> = ["c1/1", "c1/2", "c1/3", "c1/4"].into_iter().map(done_slot).collect();
    assert_eq!(summaries(&slots.lock().unwrap()), expected);
    checked.resolve();
    submission.wait(&ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
}

// Not ported: "gives callers a result with its task ID and only diagnostics for a nested call the scheduler
// faulted". Its tool returns details that are not JSON to fault the task; a Rust result is always JSON.

#[tokio::test]
async fn gives_callers_what_a_schema_less_tool_streamed_before_it_threw() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        tool("fails", |_, api, _| async move {
            api.output("half way\n")?;
            Err(DurableError::error("broke"))
        }),
    );
    let result = Arc::new(Mutex::new(None));
    let sink = result.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                *sink.lock().unwrap() = Some(api.execute_tool("fails", Default::default(), &context, plain()).await?);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let result = result.lock().unwrap().clone().unwrap();
    assert!(result.is_error);
    assert_eq!(nested_text(&result), "half way\n");
    let diagnostics: Vec<(Option<String>, String)> =
        result.diagnostics.iter().map(|diagnostic| (diagnostic.code.clone(), diagnostic.message.clone())).collect();
    assert_eq!(diagnostics, [(Some("tool_error".to_owned()), "broke".to_owned())]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn shows_a_finished_nested_call_as_a_done_slot_with_its_summary_until_its_caller_settles() {
    let setup = chat_setup(Default::default());
    let (release, checked) = (Deferred::default(), Deferred::default());
    // Diagnostics win over output for the error text, which is cut to 500 characters.
    add_tool(
        &setup,
        tool("verbose", |_, _, _| async {
            let diagnostics = vec![ToolDiagnostic::new(DiagnosticSeverity::Error, "e".repeat(600))];
            Ok(ToolExecutionResult {
                is_error: Some(true),
                diagnostics: Some(diagnostics),
                ..ToolExecutionResult::text("ignored")
            })
        }),
    );
    let usage = json!({
        "input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 3,
        "cost": { "input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3 }
    });
    let typed: pi_ai::Usage = serde_json::from_value(usage.clone()).unwrap();
    add_tool(&setup, echo_tool(Arc::default()));
    add_tool(
        &setup,
        tool(
            "paid",
            move |_, _, _| async move { Ok(ToolExecutionResult { usage: Some(typed), ..Default::default() }) },
        ),
    );
    // An error with mixed content: programs get the list, the summary its text.
    add_tool(
        &setup,
        tool("broken", |_, _, _| async {
            let image = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
            let output = vec![ContentBlock::text("bad image"), image];
            Ok(ToolExecutionResult { output: Some(output), is_error: Some(true), ..Default::default() })
        }),
    );
    let (gate, signal) = (release.clone(), checked.clone());
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let (release, checked) = (gate.clone(), signal.clone());
            async move {
                api.execute_tool("echo", args(json!({ "text": "a" })), &context, plain()).await?;
                for name in ["missing", "paid", "broken", "verbose"] {
                    api.execute_tool(name, Default::default(), &context, plain()).await?;
                }
                checked.resolve();
                release.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    setup.faux.set_responses([call("batch"), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    checked.wait().await;
    let usage: Value = serde_json::to_value(serde_json::from_value::<pi_ai::Usage>(usage).unwrap()).unwrap();
    let slot = |id: &str, summary: Value| json!({ "callId": id, "status": "done", "summary": summary });
    let expected = vec![
        slot("c1/1", json!({ "isError": false, "durationMs": "any" })),
        slot("c1/2", json!({ "isError": true, "error": "Tool missing is not available" })),
        slot("c1/3", json!({ "isError": false, "durationMs": "any", "usage": usage })),
        slot("c1/4", json!({ "isError": true, "durationMs": "any", "error": "bad image" })),
        slot("c1/5", json!({ "isError": true, "durationMs": "any", "error": "e".repeat(500) })),
    ];
    let actual = summaries(&live_value(&harness, &root).await);
    assert_eq!(Json::from(Value::Array(actual)), Json::from(Value::Array(expected)));
    release.resolve();
    submission.wait(&ctx()).await.unwrap();
    assert_eq!(live_value(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[test]
fn migrates_version_1_tool_task_input_to_a_model_issued_call() {
    let input = Json::from(json!({ "assistant": 3, "callId": "c1" }));
    let checkpoint = Json::from(json!({ "phase": "call" }));
    let (input, checkpoint) = tool_task().run_migrate(&input, &checkpoint, 1).unwrap();
    assert_eq!(input.to_serde(), json!({ "kind": "model", "assistant": 3, "callId": "c1" }));
    assert_eq!(checkpoint.to_serde(), json!({ "phase": "call" }));
}

/// Store a live tool task for call `c1` of an assistant entry, close, and rewrite its record as version 1 stored it:
/// `{ assistant, callId }` input at version 1, with `checkpoint`. Returns the storage path and the task ID.
async fn store_version_1(setup: &ChatSetup, directory: &tempfile::TempDir, checkpoint: Value) -> (String, TaskId) {
    let path = directory.path().join("session.sqlite").to_string_lossy().into_owned();
    // No submission, so the Harness never starts scheduling and the task stays pending.
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), setup).await;
    let id = root.id;
    let stage = move |tx: Tx| async move {
        let message = super::call_message("echo", json!({ "text": "old" }));
        let draft = EntryDraft {
            model: Some(vec![pi_ai::Message::Assistant(message)]),
            ..EntryDraft::new(ASSISTANT_ENTRY.kind())
        };
        let entry = tx.append_entry(id, draft).await?;
        let input = Json::from(json!({ "kind": "model", "assistant": entry.id, "callId": "c1" }));
        tx.create_task(tool_task(), input, TaskOptions::conversation(id)).await
    };
    let task_id = root.commit(stage, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();
    let storage = open_sqlite_storage(&path).await.unwrap();
    let record = storage.task(task_id, &ctx()).await.unwrap().unwrap();
    let mut input = record.input.to_serde();
    input.as_object_mut().unwrap().remove("kind");
    let mut state = serde_json::to_value(&record.state).unwrap();
    state["checkpoint"] = checkpoint;
    let old =
        TaskRecord { version: 1, input: Json::from(input), state: serde_json::from_value(state).unwrap(), ..record };
    storage.commit(&[StorageWrite::Task { value: old }], &|_| {}, &ctx()).await.unwrap();
    storage.close(&ctx()).await.unwrap();
    (path, task_id)
}

#[tokio::test]
async fn migrates_a_pending_version_1_task_and_runs_it_as_a_model_issued_call() {
    let directory = tempfile::tempdir().unwrap();
    let setup = chat_setup(Default::default());
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    add_tool(&setup, echo_tool(runs.clone()));
    let (path, id) = store_version_1(&setup, &directory, json!({ "phase": "call" })).await;
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    assert_eq!(harness.get_task(id, &ctx()).await.unwrap().unwrap().version, 1);
    harness.resume().unwrap();
    let settled = harness.wait_for_task(id, &ctx()).await.unwrap();
    assert_eq!(settled.version, 2);
    assert_eq!((settled.input["kind"].to_serde(), settled.input["callId"].to_serde()), (json!("model"), json!("c1")));
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!((outcome["status"].clone(), outcome["result"]["kind"].clone()), (json!("completed"), json!("model")));
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!((result.tool_call_id.as_str(), result_text(&result).as_str()), ("c1", "echo old"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn migrates_a_version_1_task_interrupted_after_intent_and_settles_it_interrupted() {
    let directory = tempfile::tempdir().unwrap();
    let setup = chat_setup(Default::default());
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    add_tool(&setup, echo_tool(runs.clone()));
    let intent = json!({ "phase": "execute", "arguments": { "text": "old" }, "replay": "unsafe" });
    let (path, id) = store_version_1(&setup, &directory, intent).await;
    let (harness, root) = open_chat(Arc::new(open_sqlite_storage(&path).await.unwrap()), &setup).await;
    harness.resume().unwrap();
    let settled = harness.wait_for_task(id, &ctx()).await.unwrap();
    assert_eq!(settled.version, 2);
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!((outcome["status"].clone(), outcome["result"]["kind"].clone()), (json!("failed"), json!("model")));
    assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let result = results(&all_entries(&root).await).remove(0);
    assert_eq!((result.tool_call_id.as_str(), result.is_error), ("c1", true));
    assert!(result_text(&result).contains("Tool echo was interrupted"));
    harness.close(&ctx()).await.unwrap();
}
