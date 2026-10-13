//! Port of upstream `test/harness-nested-tools.test.ts` (the core cases).

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, Message, StopReason, ToolResultMessage};
use pi_chord::Json;
use pi_durable::entries::TOOL_RESULT_ENTRY;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{BeforeToolDecision, ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_text,
    faux_tool_call_with_id,
};
use pi_durable::{EntryRecord, SubmissionStatus, TaskQuery, TaskRecord};
use serde_json::{Value, json};
use support::harness_support::*;

fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = pi_durable::DurableResult<ToolExecutionResult>> + Send + 'static,
{
    let parameters = json!({ "type": "object", "properties": { "text": { "type": "string" } } });
    define_tool(name, name, parameters, execute)
}

/// Returns `echo <text>` and counts its runs.
fn echo_tool(runs: Arc<AtomicUsize>) -> ToolRegistration {
    tool("echo", move |args, _, _| {
        runs.fetch_add(1, Ordering::SeqCst);
        let text = args.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
        async move { Ok(ToolExecutionResult::text(format!("echo {text}"))) }
    })
}

fn add_tool(setup: &ChatSetup, tool: ToolRegistration) {
    let name = format!("tool:{}", tool.name());
    setup.registry.install(Extension::new(name).tool(tool)).unwrap();
}

fn call(name: &str) -> FauxResponseStep {
    let block = faux_tool_call_with_id(name, json!({}), "c1");
    faux_assistant_message(vec![block], FauxMessageOptions::stop_reason(StopReason::ToolUse)).into()
}

fn done() -> FauxResponseStep {
    faux_assistant_message(vec![faux_text("done")], FauxMessageOptions::default()).into()
}

fn results(entries: &[EntryRecord]) -> Vec<ToolResultMessage> {
    entries
        .iter()
        .filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry)))
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn result_text(message: &ToolResultMessage) -> String {
    let parts: Vec<&str> = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.as_str(),
            _ => "",
        })
        .collect();
    parts.join("|")
}

/// The value a schema-less nested call gives programs.
fn nested_text(result: &NestedToolExecutionResult) -> String {
    result.structured_output.as_ref().and_then(Json::as_str).unwrap_or_default().to_owned()
}

async fn tool_tasks(harness: &Harness) -> Vec<TaskRecord> {
    let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
    let scan = harness.commit(move |tx| async move { tx.scan_tasks(query, 100, None).await }, &ctx());
    scan.await.unwrap().items
}

async fn run_once(
    setup: &ChatSetup,
    responses: Vec<FauxResponseStep>,
) -> (Harness, Conversation, SubmissionStatus, Vec<EntryRecord>) {
    setup.faux.set_responses(responses);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), setup).await;
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    (harness, root, settled.status, entries)
}

fn args(value: Value) -> pi_ai::JsonObject {
    value.as_object().cloned().unwrap_or_default()
}

#[tokio::test]
async fn runs_a_nested_call_as_its_own_tool_task_and_returns_its_result_to_the_caller_not_the_transcript() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_tool(&setup, echo_tool(Arc::default()));
    let nested = Arc::new(Mutex::new(None));
    let sink = nested.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                let result =
                    api.execute_tool("echo", args(json!({ "text": "a" })), &context, Default::default()).await?;
                let text = nested_text(&result);
                *sink.lock().unwrap() = Some(result);
                Ok(ToolExecutionResult::text(format!("got {text}")))
            }
        }),
    );
    let (harness, root, status, entries) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(status, SubmissionStatus::Done, "{:?}", setup.reports());
    let tasks = tool_tasks(&harness).await;
    let (parent, child) = (&tasks[0], &tasks[1]);
    let nested = nested.lock().unwrap().clone().unwrap();
    assert_eq!((nested.task_id, nested_text(&nested), nested.is_error), (child.id, "echo a".to_owned(), false));
    assert!(nested.diagnostics.is_empty() && nested.duration_ms.is_some());
    // One result entry: the model-issued call's, which records nothing of its nested calls.
    let texts: Vec<_> = results(&entries).iter().map(|r| (r.tool_call_id.clone(), result_text(r))).collect();
    assert_eq!(texts, [("c1".to_owned(), "got echo a".to_owned())]);
    let parent_input = parent.input.to_serde();
    assert_eq!((parent_input["kind"].as_str(), parent_input["callId"].as_str()), (Some("model"), Some("c1")));
    assert_eq!(child.owner, Some(parent.id));
    assert_eq!(
        child.input.to_serde(),
        json!({
            "kind": "nested",
            "parent": parent.id,
            "parentCallId": "c1",
            "key": "1",
            "call": { "type": "toolCall", "id": "c1/1", "name": "echo", "arguments": { "text": "a" } }
        })
    );
    // The receipt stays small: the result lived in the caller's documents, which retired with it.
    assert_eq!(
        serde_json::to_value(&child.state).unwrap(),
        json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "kind": "nested" } } })
    );
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn gives_each_nested_call_its_own_task_and_call_id() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    add_tool(
        &setup,
        tool("who", move |_, api, _| {
            sink.lock().unwrap().push((api.task_id, api.call_id.clone()));
            async { Ok(ToolExecutionResult::default()) }
        }),
    );
    add_tool(
        &setup,
        tool("batch", |_, api, context| async move {
            let key = |key: &str| ExecuteToolOptions { key: Some(key.into()), ..Default::default() };
            let (a, b) = futures::join!(
                api.execute_tool("who", args(json!({})), &context, key("a")),
                api.execute_tool("who", args(json!({})), &context, key("b"))
            );
            a?;
            b?;
            Ok(ToolExecutionResult::default())
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let seen = seen.lock().unwrap().clone();
    let mut calls: Vec<_> = seen.iter().map(|(_, call)| call.clone()).collect();
    calls.sort();
    assert_eq!(calls, ["c1/a", "c1/b"]);
    assert_ne!(seen[0].0, seen[1].0);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_the_tool_hooks_on_nested_calls_which_see_their_parent() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let runs = Arc::new(AtomicUsize::new(0));
    add_tool(&setup, echo_tool(runs.clone()));
    let parents = Arc::new(Mutex::new(Vec::new()));
    let sink = parents.clone();
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(move |call: ToolHookCall, _, _| {
            sink.lock().unwrap().push(call.parent.clone());
            let secret = call.parent.is_some() && call.call.arguments.get("text") == Some(&json!("secret"));
            let decision =
                secret.then(|| BeforeToolDecision { block: Some("no secrets".into()), ..Default::default() });
            Box::pin(async move { Ok(decision) })
        })),
        after_tool: Some(Arc::new(|(call, result): (ToolHookCall, ToolExecutionResult), _, _| {
            let replaced = call
                .parent
                .is_some()
                .then(|| ToolExecutionResult { output: Some(vec![ContentBlock::text("rewritten")]), ..result });
            Box::pin(async move { Ok(replaced) })
        })),
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    let nested = Arc::new(Mutex::new(Vec::new()));
    let results_sink = nested.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = results_sink.clone();
            async move {
                let first =
                    api.execute_tool("echo", args(json!({ "text": "secret" })), &context, Default::default()).await?;
                let second =
                    api.execute_tool("echo", args(json!({ "text": "public" })), &context, Default::default()).await?;
                sink.lock().unwrap().extend([first, second]);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let parent = tool_tasks(&harness).await[0].id;
    let expected = Some((parent, "c1".to_owned()));
    assert_eq!(*parents.lock().unwrap(), [None, expected.clone(), expected]);
    let nested = nested.lock().unwrap().clone();
    assert!(nested[0].is_error);
    assert_eq!(
        serde_json::to_value(&nested[0].diagnostics).unwrap(),
        json!([{ "severity": "error", "code": "blocked", "message": "Tool call blocked: no secrets" }])
    );
    assert_eq!(nested_text(&nested[1]), "rewritten");
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn returns_an_error_result_for_an_unavailable_tool_and_invalid_arguments() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_tool(&setup, echo_tool(Arc::default()));
    let nested = Arc::new(Mutex::new(Vec::new()));
    let sink = nested.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                let missing = api.execute_tool("missing", args(json!({})), &context, Default::default()).await?;
                let invalid = api
                    .execute_tool("echo", args(json!({ "text": { "not": "a string" } })), &context, Default::default())
                    .await?;
                sink.lock().unwrap().extend([missing, invalid]);
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, ..) = run_once(&setup, vec![call("batch"), done()]).await;
    let codes: Vec<_> = nested
        .lock()
        .unwrap()
        .iter()
        .map(|result| (result.is_error, result.diagnostics.first().and_then(|d| d.code.clone())))
        .collect();
    assert_eq!(codes, [(true, Some("tool_unavailable".into())), (true, Some("invalid_arguments".into()))]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn returns_the_call_a_key_already_names_and_rejects_reusing_the_key_for_another_call() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let runs = Arc::new(AtomicUsize::new(0));
    add_tool(&setup, echo_tool(runs.clone()));
    let reused = Arc::new(Mutex::new(None));
    let sink = reused.clone();
    add_tool(
        &setup,
        tool("batch", move |_, api, context| {
            let sink = sink.clone();
            async move {
                let key = || ExecuteToolOptions { key: Some("k".into()), ..Default::default() };
                api.execute_tool("echo", args(json!({ "text": "a" })), &context, key()).await?;
                let again = api.execute_tool("echo", args(json!({ "text": "a" })), &context, key()).await?;
                *sink.lock().unwrap() = Some(again);
                api.execute_tool("echo", args(json!({ "text": "b" })), &context, key()).await?;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (harness, _, _, entries) = run_once(&setup, vec![call("batch"), done()]).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(nested_text(reused.lock().unwrap().as_ref().unwrap()), "echo a");
    let text = result_text(&results(&entries)[0]);
    assert!(text.contains("Nested call c1/k was already made with another tool or other arguments"), "{text}");
    harness.close(&ctx()).await.unwrap();
}
