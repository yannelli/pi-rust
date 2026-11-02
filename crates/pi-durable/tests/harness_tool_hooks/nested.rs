//! Upstream `describe("afterTool on results the call phase decides")`: the nested calls.

use std::sync::{Arc, Mutex};

use pi_ai::ContentBlock;
use pi_chord::Json;
use pi_durable::harness::tool::{ToolHookCall, ToolHooks};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use serde_json::{Value, json};

use super::support::harness_support::*;
use super::{Seen, add_hooks, add_tool, calls, codes, decision, done, empty, recording, submit_and_wait, tool};

/// A `batch` tool that makes the nested `calls` in order and collects their results.
pub fn batch(calls: Vec<(&'static str, Value)>) -> (ToolRegistration, Arc<Mutex<Vec<NestedToolExecutionResult>>>) {
    let nested = Arc::new(Mutex::new(Vec::new()));
    let sink = nested.clone();
    let registration = tool("batch", move |_, api, context| {
        let (sink, calls) = (sink.clone(), calls.clone());
        async move {
            for (name, args) in calls {
                let args = args.as_object().cloned().unwrap_or_default();
                let result = api.execute_tool(name, args, &context, ExecuteToolOptions::default()).await?;
                sink.lock().unwrap().push(result);
            }
            Ok(ToolExecutionResult::default())
        }
    });
    (registration, nested)
}

#[tokio::test]
async fn runs_on_nested_invalid_and_blocked_calls_the_caller_gets_diagnostics_and_details_not_output() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("echo", |_, _, _| async { Ok(empty()) }));
    let (registration, nested) = batch(vec![
        ("echo", json!({ "text": { "not": "a string" } })),
        ("echo", json!({ "text": "secret" })),
        ("missing", json!({})),
    ]);
    add_tool(&setup, registration);
    let seen = Seen::default();
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            let secret = call.call.arguments.get("text") == Some(&json!("secret"));
            let decided = if secret { decision(Some("no secrets"), None) } else { None };
            Box::pin(async move { Ok(decided) })
        })),
        after_tool: Some(recording(&seen, |call, result| {
            call.parent.as_ref()?;
            let mut diagnostics = result.diagnostics.clone().unwrap_or_default();
            diagnostics.push(ToolDiagnostic::new(DiagnosticSeverity::Info, "seen").with_code("hooked"));
            Some(ToolExecutionResult {
                output: Some(vec![ContentBlock::text("dropped")]),
                structured_output: Some(Json::from(json!({ "dropped": true }))),
                details: Some(Json::from(json!({ "hooked": call.call.id }))),
                diagnostics: Some(diagnostics),
                ..result.clone()
            })
        })),
    };
    add_hooks(&setup, hooks);
    setup.faux.set_responses([calls(&[("batch", json!({}), "c1")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    submit_and_wait(&root, "go").await;
    let seen = seen.lock().unwrap().clone();
    let nested_calls: Vec<_> =
        seen.iter().filter_map(|(call, _)| call.parent.as_ref().map(|parent| (call, parent))).collect();
    let ids: Vec<&str> = nested_calls.iter().map(|(call, _)| call.call.id.as_str()).collect();
    assert_eq!(ids, ["c1/1", "c1/2", "c1/3"]);
    assert!(nested_calls.iter().all(|(_, parent)| parent.1 == "c1"));
    let nested = nested.lock().unwrap().clone();
    let outcomes: Vec<(bool, Vec<String>)> =
        nested.iter().map(|result| (result.is_error, codes(&result.diagnostics))).collect();
    let expected = [["invalid_arguments", "hooked"], ["blocked", "hooked"], ["tool_unavailable", "hooked"]];
    let expected: Vec<(bool, Vec<String>)> =
        expected.iter().map(|codes| (true, codes.iter().map(|code| (*code).to_owned()).collect())).collect();
    assert_eq!(outcomes, expected);
    let details: Vec<Option<Value>> = nested.iter().map(|result| result.details.as_ref().map(Json::to_serde)).collect();
    assert_eq!(
        details,
        [Some(json!({ "hooked": "c1/1" })), Some(json!({ "hooked": "c1/2" })), Some(json!({ "hooked": "c1/3" }))]
    );
    // A Harness error result has no structured output, even when a hook supplies one.
    assert_eq!(nested[1].structured_output, None);
    assert_eq!(nested[2].structured_output, None);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn gives_a_nested_call_a_hook_turns_into_a_success_its_output_as_value_unless_the_tool_has_a_schema() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, tool("plain", |_, _, _| async { Ok(empty()) }));
    let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } }, "required": ["ok"] });
    let typed = tool("typed", |_, _, _| async {
        Ok(ToolExecutionResult { structured_output: Some(Json::from(json!({ "ok": true }))), ..empty() })
    });
    add_tool(&setup, typed.with_structured_output_schema(schema));
    let (registration, nested) = batch(vec![("plain", json!({})), ("typed", json!({}))]);
    add_tool(&setup, registration);
    let hooks = ToolHooks {
        before_tool: Some(Arc::new(|call: ToolHookCall, _, _| {
            let decided = call.parent.as_ref().and_then(|_| decision(Some("no"), None));
            Box::pin(async move { Ok(decided) })
        })),
        after_tool: Some(recording(&Seen::default(), |call, _| {
            call.parent.as_ref()?;
            Some(ToolExecutionResult {
                output: Some(vec![ContentBlock::text(format!("fine {}", call.call.name))]),
                structured_output: Some(Json::from(json!({ "ok": false }))),
                ..ToolExecutionResult::default()
            })
        })),
    };
    add_hooks(&setup, hooks);
    setup.faux.set_responses([calls(&[("batch", json!({}), "c1")]), done()]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    submit_and_wait(&root, "go").await;
    let outcomes: Vec<(bool, Option<Value>)> = nested
        .lock()
        .unwrap()
        .iter()
        .map(|result| (result.is_error, result.structured_output.as_ref().map(Json::to_serde)))
        .collect();
    assert_eq!(outcomes, [(false, Some(json!("fine plain"))), (false, None)]);
    harness.close(&ctx()).await.unwrap();
}
