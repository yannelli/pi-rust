//! Port of upstream `test/harness-tool-hooks.test.ts`: `after_tool` runs on every result a tool task writes, also
//! those the Harness decides without executing the tool (upstream issue 11043).

#[path = "harness_tool_hooks/limits.rs"]
mod limits;
#[path = "harness_tool_hooks/nested.rs"]
mod nested;
#[path = "harness_tool_hooks/slot.rs"]
mod slot;
mod support;
#[path = "harness_tool_hooks/unoffered.rs"]
mod unoffered;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use pi_ai::{ContentBlock, Message, StopReason, ToolResultMessage};
use pi_chord::{Context, Json};
use pi_durable::entries::TOOL_RESULT_ENTRY;
use pi_durable::harness::tool::{BeforeToolDecision, ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call_with_id,
};
use pi_durable::{DurableResult, EntryRecord, SubmissionStatus};
use serde_json::{Value, json};
use support::harness_support::*;

pub type Seen = Arc<Mutex<Vec<(ToolHookCall, ToolExecutionResult)>>>;
pub type AfterTool = Arc<
    dyn Fn(
            (ToolHookCall, ToolExecutionResult),
            HookApi,
            Context,
        ) -> BoxFuture<'static, DurableResult<Option<ToolExecutionResult>>>
        + Send
        + Sync,
>;

pub fn tool<F, Fut>(name: &str, execute: F) -> ToolRegistration
where
    F: Fn(pi_ai::JsonObject, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = DurableResult<ToolExecutionResult>> + Send + 'static,
{
    let parameters = json!({ "type": "object", "properties": { "text": { "type": "string" } } });
    define_tool(name, name, parameters, execute)
}

pub fn empty() -> ToolExecutionResult {
    ToolExecutionResult { output: Some(Vec::new()), ..ToolExecutionResult::default() }
}

pub fn add_tool(setup: &ChatSetup, tool: ToolRegistration) {
    setup.registry.install(Extension::new(format!("tool:{}", tool.name())).tool(tool)).unwrap();
}

pub fn add_hooks(setup: &ChatSetup, hooks: ToolHooks) {
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
}

/// An `after_tool` hook that records what it sees and returns what `replace` makes of it.
pub fn recording(
    seen: &Seen,
    replace: impl Fn(&ToolHookCall, &ToolExecutionResult) -> Option<ToolExecutionResult> + Send + Sync + 'static,
) -> AfterTool {
    let seen = seen.clone();
    Arc::new(move |(call, result): (ToolHookCall, ToolExecutionResult), _: HookApi, _: Context| {
        let replaced = replace(&call, &result);
        seen.lock().unwrap().push((call, result));
        Box::pin(async move { Ok(replaced) })
    })
}

pub fn call_message(list: &[(&str, Value, &str)]) -> pi_ai::AssistantMessage {
    let blocks: Vec<_> = list.iter().map(|(name, args, id)| faux_tool_call_with_id(*name, args.clone(), *id)).collect();
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::ToolUse))
}

pub fn calls(list: &[(&str, Value, &str)]) -> FauxResponseStep {
    call_message(list).into()
}

pub fn done() -> FauxResponseStep {
    faux_assistant_message(vec![faux_text("done")], FauxMessageOptions::default()).into()
}

/// A faux response that first changes the root's tools, as a host may while a request is out.
pub fn after_configuring(
    root: &Arc<Mutex<Option<Conversation>>>,
    tools: ToolFilter,
    message: pi_ai::AssistantMessage,
) -> FauxResponseStep {
    let root = root.clone();
    FauxResponseStep::async_factory(move |_, _, _, _| {
        let (root, tools, message) = (root.lock().unwrap().clone().unwrap(), tools.clone(), message.clone());
        Box::pin(async move {
            let change = AgentChange { tools: Change::Set(tools), ..AgentChange::default() };
            root.configure(change, &ctx()).await.map_err(|error| error.to_string())?;
            Ok(message)
        })
    })
}

pub fn results(entries: &[EntryRecord]) -> HashMap<String, ToolResultMessage> {
    let messages = entries.iter().filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry))).filter_map(|entry| {
        match entry.model.as_ref()?.first()? {
            Message::ToolResult(result) => Some((result.tool_call_id.clone(), result.clone())),
            _ => None,
        }
    });
    messages.collect()
}

pub fn text(content: &[ContentBlock]) -> String {
    content.iter().map(|block| block.as_text().unwrap_or_default()).collect::<Vec<_>>().join("|")
}

pub fn codes(diagnostics: &[ToolDiagnostic]) -> Vec<String> {
    diagnostics.iter().map(|diagnostic| diagnostic.code.clone().unwrap_or_default()).collect()
}

pub fn result_codes(result: &ToolExecutionResult) -> Vec<String> {
    codes(result.diagnostics.as_deref().unwrap_or_default())
}

/// The diagnostic codes a result entry's `data` keeps.
pub fn diagnostics_of(entries: &[EntryRecord], call_id: &str) -> Vec<String> {
    let answers = |entry: &&EntryRecord| match entry.model.as_ref().and_then(|model| model.first()) {
        Some(Message::ToolResult(result)) => result.tool_call_id == call_id,
        _ => false,
    };
    let entry = entries.iter().filter(|entry| TOOL_RESULT_ENTRY.is(Some(entry))).find(answers);
    let data = entry.and_then(|entry| entry.data.as_ref()).map(Json::to_serde).unwrap_or(Value::Null);
    let diagnostics: Vec<ToolDiagnostic> = serde_json::from_value(data["diagnostics"].clone()).unwrap_or_default();
    codes(&diagnostics)
}

pub async fn submit_and_wait(root: &Conversation, text: &str) -> SubmissionStatus {
    root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().wait(&ctx()).await.unwrap().status
}

pub fn decision(block: Option<&str>, arguments: Option<Value>) -> Option<BeforeToolDecision> {
    let arguments = arguments.and_then(|value| value.as_object().cloned());
    Some(BeforeToolDecision { block: block.map(str::to_owned), arguments })
}

#[tokio::test]
async fn sees_and_replaces_unavailable_invalid_and_blocked_results_with_the_raw_arguments() {
    let setup = chat_setup(Default::default());
    let ran = Arc::new(AtomicUsize::new(0));
    let count = ran.clone();
    add_tool(
        &setup,
        tool("echo", move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Ok(ToolExecutionResult::text("ran")) }
        }),
    );
    add_tool(&setup, tool("gone", |_, _, _| async { Ok(empty()) }));
    let seen = Seen::default();
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            let decided = match call.call.id.as_str() {
                "block" => decision(Some("not today"), None),
                "rewrite" => decision(None, Some(json!({ "text": { "not": "a string" } }))),
                _ => None,
            };
            Box::pin(async move { Ok(decided) })
        })),
        after_tool: Some(recording(&seen, |call, result| {
            let output = Some(vec![ContentBlock::text(format!("hooked {}", call.call.id))]);
            Some(ToolExecutionResult { output, details: Some(Json::from(json!({ "by": "hook" }))), ..result.clone() })
        })),
    };
    add_hooks(&setup, hooks);
    let root_slot = Arc::default();
    let round = call_message(&[
        ("gone", json!({}), "unavailable"),
        ("echo", json!({ "text": { "not": "a string" } }), "invalid"),
        ("echo", json!({ "text": "x" }), "rewrite"),
        ("echo", json!({}), "block"),
    ]);
    // The tool is offered but no longer resolves when its task runs.
    let only_echo = ToolFilter::List(vec!["echo".into()]);
    setup.faux.set_responses([after_configuring(&root_slot, only_echo, round), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *root_slot.lock().unwrap() = Some(root.clone());
    assert_eq!(submit_and_wait(&root, "go").await, SubmissionStatus::Done);
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    let by_id: HashMap<String, (ToolHookCall, ToolExecutionResult)> =
        seen.lock().unwrap().iter().map(|seen| (seen.0.call.id.clone(), seen.clone())).collect();
    let mut ids: Vec<&str> = by_id.keys().map(String::as_str).collect();
    ids.sort();
    assert_eq!(ids, ["block", "invalid", "rewrite", "unavailable"]);
    assert_eq!(result_codes(&by_id["unavailable"].1), ["tool_unavailable"]);
    assert_eq!(result_codes(&by_id["invalid"].1), ["invalid_arguments"]);
    assert_eq!(result_codes(&by_id["rewrite"].1), ["invalid_arguments"]);
    assert_eq!(result_codes(&by_id["block"].1), ["blocked"]);
    // Hooks see the call as the model made it, also when it is invalid or `before_tool` rewrote it.
    assert_eq!(Value::Object(by_id["invalid"].0.call.arguments.clone()), json!({ "text": { "not": "a string" } }));
    assert_eq!(Value::Object(by_id["rewrite"].0.call.arguments.clone()), json!({ "text": "x" }));
    let messages = results(&all_entries(&root).await);
    for id in ["unavailable", "invalid", "rewrite", "block"] {
        let message = &messages[id];
        assert!(message.is_error);
        assert_eq!(message.details, Some(json!({ "by": "hook" })));
        assert!(text(&message.content).starts_with(&format!("hooked {id}|<harness>")), "{:?}", message.content);
    }
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn bounds_a_replacement_output_with_the_tool_s_limits_or_the_defaults_for_an_unresolved_tool() {
    let setup = chat_setup(Default::default());
    let limits = ToolOutputLimits { max_bytes: Some(10), ..ToolOutputLimits::default() };
    add_tool(&setup, tool("small", |_, _, _| async { Ok(empty()) }).with_output_limits(limits));
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|_: ToolHookCall, _, _| Box::pin(async { Ok(decision(Some("no"), None)) }))),
        after_tool: Some(recording(&Seen::default(), |call, result| {
            let text = if call.call.id == "c2" { "x".repeat(100) } else { "y".repeat(60 * 1024) };
            Some(ToolExecutionResult { output: Some(vec![ContentBlock::text(text)]), ..result.clone() })
        })),
    };
    add_hooks(&setup, hooks);
    let root_slot = Arc::default();
    let round = call_message(&[("small", json!({}), "c1")]);
    setup.faux.set_responses([after_configuring(&root_slot, ToolFilter::List(Vec::new()), round), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    *root_slot.lock().unwrap() = Some(root.clone());
    submit_and_wait(&root, "go").await;
    root.configure(AgentChange { tools: Change::Clear, ..AgentChange::default() }, &ctx()).await.unwrap();
    setup.faux.set_responses([calls(&[("small", json!({}), "c2")]), done()]);
    submit_and_wait(&root, "again").await;
    let entries = all_entries(&root).await;
    let messages = results(&entries);
    // Unresolved: the default 50 KB.
    assert_eq!(messages["c1"].content[0].as_text().map(str::len), Some(50 * 1024));
    assert_eq!(diagnostics_of(&entries, "c1"), ["tool_unavailable", "truncated"]);
    assert_eq!(messages["c2"].content[0].as_text(), Some("x".repeat(10).as_str()));
    assert_eq!(diagnostics_of(&entries, "c2"), ["blocked", "truncated"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn lets_a_hook_s_terminate_on_a_blocked_call_count_toward_the_round() {
    let setup = chat_setup(Default::default());
    let terminate = || Some(ToolControl { terminate: Some(true), ..ToolControl::default() });
    add_tool(
        &setup,
        tool("echo", move |_, _, _| async move { Ok(ToolExecutionResult { control: terminate(), ..empty() }) }),
    );
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            let decided = if call.call.id == "c2" { decision(Some("no"), None) } else { None };
            Box::pin(async move { Ok(decided) })
        })),
        after_tool: Some(recording(&Seen::default(), move |_, result| {
            Some(ToolExecutionResult { control: terminate(), ..result.clone() })
        })),
    };
    add_hooks(&setup, hooks);
    setup.faux.set_responses([calls(&[("echo", json!({}), "c1"), ("echo", json!({}), "c2")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    assert_eq!(submit_and_wait(&root, "go").await, SubmissionStatus::Done);
    // The round terminated: no second request.
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}
