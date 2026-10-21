//! Port of upstream `test/harness-tools.test.ts`. This file holds the helpers and the first cases; the modules in
//! `harness_tools/` hold the other cases, one per upstream `describe` group.

#[path = "harness_tools/hooks.rs"]
mod hooks;
#[path = "harness_tools/lifetime.rs"]
mod lifetime;
#[path = "harness_tools/results.rs"]
mod result_cases;
#[path = "harness_tools/rounds.rs"]
mod rounds;
mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::{ContentBlock, Message, StopReason, ToolResultMessage};
use pi_chord::Json;
use pi_durable::entries::TOOL_RESULT_ENTRY;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::tool::{BeforeToolDecision, ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::models::Models;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message, faux_text,
    faux_tool_call_with_id,
};
use pi_durable::{EntryRecord, SubmissionStatus};
use serde_json::{Value, json};
use support::harness_support::*;

pub fn echo_parameters() -> Value {
    json!({ "type": "object", "properties": { "text": { "type": "string" } } })
}

pub fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, pi_chord::Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = pi_durable::DurableResult<ToolExecutionResult>> + Send + 'static,
{
    define_tool(name, format!("The {name} tool"), echo_parameters(), execute)
}

pub fn empty_result() -> ToolExecutionResult {
    ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() }
}

pub fn add_tool(setup: &ChatSetup, tool: ToolRegistration) {
    let name = format!("tool:{}", tool.name());
    setup.registry.install(Extension::new(name).tool(tool)).unwrap();
}

pub fn add_tool_hooks(setup: &ChatSetup, name: &str, hooks: ToolHooks) {
    setup.registry.install(Extension::new(name).hook(hook(&tool_task(), hooks))).unwrap();
}

/// A tool-calling answer with one call per `(name, args, id)`.
pub fn call_message(list: &[(&str, Value, &str)]) -> pi_ai::AssistantMessage {
    let blocks =
        list.iter().map(|(name, args, id)| faux_tool_call_with_id(*name, args.clone(), *id)).collect::<Vec<_>>();
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse))
}

pub fn calls(list: &[(&str, Value, &str)]) -> FauxResponseStep {
    call_message(list).into()
}

pub fn done() -> FauxResponseStep {
    faux_assistant_message(vec![faux_text("done")], FauxMessageOptions::default()).into()
}

pub struct Run {
    pub harness: Harness,
    pub root: Conversation,
    pub entries: Vec<EntryRecord>,
    pub status: SubmissionStatus,
}

pub async fn run(setup: &ChatSetup, responses: Vec<FauxResponseStep>) -> Run {
    setup.faux.set_responses(responses);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), setup).await;
    let settled = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let entries = all_entries(&root).await;
    Run { harness, root, entries, status: settled.status }
}

pub fn results(entries: &[EntryRecord]) -> Vec<ToolResultMessage> {
    entries
        .iter()
        .filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry)))
        .filter_map(|entry| match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .collect()
}

pub fn result_text(message: &ToolResultMessage) -> String {
    let parts: Vec<String> = message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text, .. } => text.clone(),
            ContentBlock::Image { .. } => "[image]".into(),
            _ => "[other]".into(),
        })
        .collect();
    parts.join("|")
}

#[tokio::test]
async fn runs_input_tool_call_tool_result_and_answer_and_settles_the_input() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_tool(
        &setup,
        tool("echo", |args, _, _| async move {
            let text = args.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            Ok(ToolExecutionResult::text(format!("echo {text}")))
        }),
    );
    let Run { harness, root, entries, status } =
        run(&setup, vec![calls(&[("echo", json!({ "text": "hi" }), "c1")]), done()]).await;
    assert_eq!(status, SubmissionStatus::Done, "{:?}", setup.reports());
    assert_eq!(kinds(&entries), ["pi.user", "pi.system", "pi.assistant", "pi.tool-result", "pi.assistant"]);
    let Some(Message::System(system)) = entries[1].model.as_ref().and_then(|model| model.first()) else {
        panic!("expected a system message");
    };
    let added: Vec<_> =
        system.tools_added.iter().flatten().map(|tool| (tool.name.clone(), tool.description.clone())).collect();
    assert_eq!(added, [("echo".to_owned(), "The echo tool".to_owned())]);
    let result = &results(&entries)[0];
    assert_eq!((result.tool_call_id.as_str(), result.tool_name.as_str(), result.is_error), ("c1", "echo", false));
    assert_eq!(result_text(result), "echo hi");
    assert_eq!(entries[3].data.as_ref().map(Json::to_serde), Some(json!({ "diagnostics": [] })));
    assert!(entries[3].by_task_id.is_some());
    assert_eq!(setup.faux.state().call_count, 2);
    assert_eq!(harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap(), Some(Json::from(json!({}))));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn gives_tools_and_hooks_the_harness_models() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool_seen = seen.clone();
    add_tool(
        &setup,
        tool("echo", move |_, api, _| {
            tool_seen.lock().unwrap().push(api.models());
            async { Ok(empty_result()) }
        }),
    );
    let hook_seen = seen.clone();
    let before: ToolHooks = ToolHooks {
        before_tool: Some(Arc::new(move |_, api: HookApi, _| {
            hook_seen.lock().unwrap().push(api.models());
            Box::pin(async { Ok(None) })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks:1", before);
    let Run { harness, status, .. } = run(&setup, vec![calls(&[("echo", json!({}), "c1")]), done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    let seen = seen.lock().unwrap().clone();
    // One view of the host's Models that adds the Harness identity, shared by tools and hooks.
    assert_eq!(seen.len(), 2);
    assert!(Arc::ptr_eq(&seen[0], &seen[1]));
    let ids = |models: &Arc<dyn Models>| models.get_models(None).into_iter().map(|model| model.id).collect::<Vec<_>>();
    assert_eq!(ids(&seen[0]), ids(&setup.models));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_calls_to_tools_the_request_did_not_offer_without_a_task() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(empty_result()) }));
    let responses = vec![calls(&[("ghost", json!({}), "c1"), ("echo", json!({}), "c2")]), done()];
    let Run { harness, root, entries, status } = run(&setup, responses).await;
    assert_eq!(status, SubmissionStatus::Done);
    let results = results(&entries);
    let ghost = results.iter().find(|result| result.tool_call_id == "c1").unwrap();
    assert!(ghost.is_error);
    assert_eq!(result_text(ghost), "<harness>\n[error] Tool ghost is not available\n</harness>");
    let echo = results.iter().find(|result| result.tool_call_id == "c2").unwrap();
    assert!(!echo.is_error);
    let ghost_entry = entries
        .iter()
        .find(|entry| matches!(entry.model.as_ref().and_then(|m| m.first()), Some(Message::ToolResult(r)) if r.tool_call_id == "c1"))
        .unwrap();
    assert_eq!(
        ghost_entry.data.as_ref().map(Json::to_serde),
        Some(
            json!({ "diagnostics": [{ "severity": "error", "code": "tool_unavailable", "message": "Tool ghost is not available" }] })
        )
    );
    let tasks = conversation_tasks(&harness, &root).await;
    assert_eq!(tasks.iter().filter(|task| task.kind == "pi.tool").count(), 1);
    harness.close(&ctx()).await.unwrap();
}

async fn trace(setup: ChatSetup) -> Vec<String> {
    let events = Arc::new(Mutex::new(Vec::new()));
    for name in ["a", "b"] {
        let events = events.clone();
        add_tool(
            &setup,
            tool(name, move |_, _, _| {
                let events = events.clone();
                async move {
                    events.lock().unwrap().push(format!("start {name}"));
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    events.lock().unwrap().push(format!("end {name}"));
                    Ok(empty_result())
                }
            }),
        );
    }
    let result = run(&setup, vec![calls(&[("a", json!({}), "c1"), ("b", json!({}), "c2")]), done()]).await;
    result.harness.close(&ctx()).await.unwrap();
    events.lock().unwrap().clone()
}

#[tokio::test]
async fn runs_a_round_in_parallel_by_default_and_sequentially_when_configured_or_required_by_a_tool() {
    let parallel = trace(chat_setup(RegisterFauxProviderOptions::default())).await;
    assert_eq!(parallel[..2], ["start a", "start b"]);
    let configured = chat_setup(RegisterFauxProviderOptions::default());
    configured.settings.lock().unwrap().tool_execution = Some(ToolExecutionMode::Sequential);
    assert_eq!(trace(configured).await, ["start a", "end a", "start b", "end b"]);

    let per_tool = chat_setup(RegisterFauxProviderOptions::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let (a_events, b_events) = (events.clone(), events.clone());
    let a = tool("a", move |_, _, _| {
        let events = a_events.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            events.lock().unwrap().push("a");
            Ok(empty_result())
        }
    });
    add_tool(&per_tool, a.with_execution_mode(ToolExecutionMode::Sequential));
    add_tool(
        &per_tool,
        tool("b", move |_, _, _| {
            b_events.lock().unwrap().push("b");
            async { Ok(empty_result()) }
        }),
    );
    let result = run(&per_tool, vec![calls(&[("a", json!({}), "c1"), ("b", json!({}), "c2")]), done()]).await;
    let tasks = conversation_tasks(&result.harness, &result.root).await;
    let generation = tasks.iter().find(|task| task.kind == "pi.generation").unwrap();
    let owners: Vec<_> = tasks.iter().filter(|task| task.kind == "pi.tool").map(|task| task.owner).collect();
    // The generation owns both tools and creates the second only after the first ended.
    assert_eq!(owners, [Some(generation.id), Some(generation.id)]);
    assert_eq!(*events.lock().unwrap(), ["a", "b"]);
    result.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn uses_retained_output_and_the_last_details_when_the_result_omits_them_with_diagnostics_in_order() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let log = tool("log", |_, api, context| async move {
        api.output("line 1\n")?;
        api.output_bytes(b"line 2\nline 3\n", None)?;
        api.diagnostic(ToolDiagnostic::new(DiagnosticSeverity::Info, "from api"))?;
        api.details(Json::from(json!({ "step": 1 })), &context).await?;
        api.details(Json::from(json!({ "step": 2 })), &context).await?;
        let diagnostics = vec![ToolDiagnostic::new(DiagnosticSeverity::Warn, "from result")];
        Ok(ToolExecutionResult { diagnostics: Some(diagnostics), ..ToolExecutionResult::default() })
    });
    add_tool(&setup, log.with_output_limits(ToolOutputLimits { max_lines: Some(2), ..ToolOutputLimits::default() }));
    let Run { harness, entries, .. } = run(&setup, vec![calls(&[("log", json!({}), "c1")]), done()]).await;
    let result = &results(&entries)[0];
    assert_eq!(
        result.details.as_ref().map(|details| serde_json::to_value(details).unwrap()),
        Some(json!({ "step": 2 }))
    );
    assert_eq!(
        result_text(result),
        "line 1\nline 2\n|<harness>\n[info] from api\n[warn] from result\n[warn] Output truncated to its beginning: 1 lines, 7 bytes dropped\n</harness>"
    );
    let entry = entries.iter().find(|entry| TOOL_RESULT_ENTRY.is(Some(entry))).unwrap();
    let codes: Vec<Value> = entry.data.as_ref().unwrap().to_serde()["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|diagnostic| diagnostic.get("code").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(codes, [Value::Null, Value::Null, json!("truncated")]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn turns_a_throw_into_a_tool_error_result_with_the_partial_output() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_tool(
        &setup,
        tool("fail", |_, api, _| async move {
            api.output("partial\n")?;
            Err(pi_durable::DurableError::error("boom"))
        }),
    );
    let Run { harness, entries, status, .. } = run(&setup, vec![calls(&[("fail", json!({}), "c1")]), done()]).await;
    assert_eq!(status, SubmissionStatus::Done);
    let result = &results(&entries)[0];
    assert!(result.is_error);
    assert_eq!(result_text(result), "partial\n|<harness>\n[error] boom\n</harness>");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn validates_arguments_before_and_after_before_tool_and_applies_blocks_and_replacements() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tool_seen = seen.clone();
    add_tool(
        &setup,
        tool("echo", move |args, _, _| {
            tool_seen.lock().unwrap().push(Value::Object(args));
            async { Ok(empty_result()) }
        }),
    );
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            let decision = match call.call.id.as_str() {
                "block" => Ok(Some(BeforeToolDecision { block: Some("not today".into()), ..Default::default() })),
                "throw" => Err(pi_durable::DurableError::error("hook failed")),
                "bad" => {
                    let arguments = json!({ "text": { "not": "a string" } }).as_object().cloned();
                    Ok(Some(BeforeToolDecision { arguments, ..Default::default() }))
                }
                _ => {
                    let text = call.call.arguments.get("text").and_then(Value::as_str).unwrap_or_default();
                    let arguments = json!({ "text": format!("{text}!") }).as_object().cloned();
                    Ok(Some(BeforeToolDecision { arguments, ..Default::default() }))
                }
            };
            Box::pin(async move { decision })
        })),
        ..ToolHooks::default()
    };
    add_tool_hooks(&setup, "hooks:1", hooks);
    let round = calls(&[
        ("echo", json!({ "text": 1 }), "coerced"),
        ("echo", json!({ "text": { "not": "a string" } }), "invalid"),
        ("echo", json!({}), "block"),
        ("echo", json!({}), "throw"),
        ("echo", json!({ "text": "x" }), "bad"),
        ("echo", json!({ "text": "x" }), "ok"),
    ]);
    let Run { harness, entries, .. } = run(&setup, vec![round, done()]).await;
    let by_id = |id: &str| {
        let result = results(&entries).into_iter().find(|result| result.tool_call_id == id).unwrap();
        (result.is_error, result_text(&result))
    };
    assert_eq!(by_id("block"), (true, "<harness>\n[error] Tool call blocked: not today\n</harness>".into()));
    assert_eq!(by_id("throw"), (true, "<harness>\n[error] Tool call blocked: hook failed\n</harness>".into()));
    let (bad_error, bad_text) = by_id("bad");
    assert!(bad_error && bad_text.contains("Validation failed"), "{bad_text}");
    assert_eq!(by_id("ok"), (false, String::new()));
    let (invalid_error, invalid_text) = by_id("invalid");
    assert!(invalid_error && invalid_text.contains("Validation failed"), "{invalid_text}");
    // pi-ai coerces a number to a string before the first validation.
    assert_eq!(by_id("coerced"), (false, String::new()));
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by_key(Value::to_string);
    assert_eq!(seen, [json!({ "text": "1!" }), json!({ "text": "x!" })]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_commits_memos_and_child_tasks_through_the_execution_api() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let memo = Arc::new(Mutex::new(Vec::new()));
    let seen = memo.clone();
    add_tool(
        &setup,
        tool("memo", move |_, api, context| {
            let seen = seen.clone();
            async move {
                let first = api.memo_or("pick", Json::from(json!(1)), &context).await?;
                let second = api.memo_or("pick", Json::from(json!(2)), &context).await?;
                let stored = api.memo("pick").await?.map(|value| value.to_serde());
                seen.lock().unwrap().push((first.to_serde(), second.to_serde(), stored));
                let (conversation_id, task_id) = (api.conversation_id, api.task_id);
                let entry = api
                    .commit(
                        move |tx| async move { tx.append_entry(conversation_id, pi_durable::EntryDraft::new("note")).await },
                        &context,
                    )
                    .await?;
                assert_eq!(entry.by_task_id, Some(task_id));
                Ok(ToolExecutionResult::text("ok"))
            }
        }),
    );
    let Run { harness, entries, status, .. } = run(&setup, vec![calls(&[("memo", json!({}), "c1")]), done()]).await;
    assert_eq!(status, SubmissionStatus::Done, "{:?}", setup.reports());
    assert_eq!(*memo.lock().unwrap(), [(json!(1), json!(1), Some(json!(1)))]);
    assert!(kinds(&entries).contains(&"note".to_owned()));
    harness.close(&ctx()).await.unwrap();
}
