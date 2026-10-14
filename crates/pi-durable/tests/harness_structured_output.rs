//! Port of upstream `test/harness-structured-output.test.ts`.

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::{ContentBlock, Message, ToolResultMessage};
use pi_durable::StorageWrite;
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv};
use pi_durable::harness::tool::{ToolHookCall, ToolHooks, tool_task};
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::tools::{BashToolOptions, create_bash_tool};
use pi_durable::types::DocumentContent;
use serde_json::{Value, json};
use support::ControlledStorage;
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::*;

fn count_schema() -> Value {
    json!({ "type": "object", "properties": { "count": { "type": "number" } }, "required": ["count"] })
}

/// A tool that returns `result`.
fn returning(name: &str, result: ToolExecutionResult) -> ToolRegistration {
    define_tool(name, name, no_parameters(), move |_, _, _| {
        let result = result.clone();
        async move { Ok(result) }
    })
}

fn text(text: &str) -> ContentBlock {
    ContentBlock::text(text)
}

fn output(blocks: Vec<ContentBlock>) -> ToolExecutionResult {
    ToolExecutionResult { output: Some(blocks), ..ToolExecutionResult::default() }
}

fn structured(value: Value) -> ToolExecutionResult {
    ToolExecutionResult { structured_output: Some(value.into()), ..ToolExecutionResult::default() }
}

/// A probe tool that runs each `(name, args)` as a nested call and records what it got back.
fn probe(calls: Vec<(&'static str, Value)>) -> (ToolRegistration, Arc<Mutex<Vec<NestedToolExecutionResult>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    let tool = define_tool("probe", "probe", no_parameters(), move |_, api, context| {
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
    (tool, received)
}

async fn go(root: &Conversation) {
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
}

/// Run `name` as a nested call of a `probe` tool and return what the probe received.
async fn nested(setup: &ChatSetup, name: &'static str) -> NestedToolExecutionResult {
    let (tool, received) = probe(vec![(name, json!({}))]);
    add_tool(setup, tool);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), setup).await;
    go(&root).await;
    harness.close(&ctx()).await.unwrap();
    received.lock().unwrap().remove(0)
}

/// Run `name` as a model-issued call and return its transcript message.
async fn direct(setup: &ChatSetup, name: &str) -> ToolResultMessage {
    setup.faux.set_responses([call(name, "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), setup).await;
    go(&root).await;
    let entries = all_entries(&root).await;
    harness.close(&ctx()).await.unwrap();
    let entry = entries.into_iter().find(|entry| entry.kind == "pi.tool-result").unwrap();
    match entry.model.unwrap().remove(0) {
        Message::ToolResult(result) => result,
        other => panic!("expected a tool result, got {other:?}"),
    }
}

fn codes(result: &NestedToolExecutionResult) -> Vec<Option<String>> {
    result.diagnostics.iter().map(|diagnostic| diagnostic.code.clone()).collect()
}

fn value(result: &NestedToolExecutionResult) -> Option<Value> {
    result.structured_output.as_ref().map(Value::from)
}

#[tokio::test]
async fn gives_callers_a_schema_less_tool_s_output_one_text_as_a_string_one_image_as_itself_else_the_list() {
    let image = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
    let image_json = json!({ "type": "image", "data": "AAAA", "mimeType": "image/png" });
    let text_json = json!({ "type": "text", "text": "one" });
    let cases = [
        (vec![text("one")], json!("one")),
        (vec![image.clone()], image_json.clone()),
        (Vec::new(), json!("")),
        (vec![text("one"), image], json!([text_json, image_json])),
        (vec![text("one"), text("one")], json!([text_json, text_json])),
    ];
    for (blocks, expected) in cases {
        let setup = chat_setup(Default::default());
        add_tool(&setup, returning("plain", output(blocks)));
        let received = nested(&setup, "plain").await;
        assert_eq!(value(&received), Some(expected));
        assert!(!received.is_error && received.diagnostics.is_empty());
        assert_eq!((received.details.as_ref(), received.usage.as_ref()), (None, None));
    }
}

#[tokio::test]
async fn gives_callers_the_bounded_output_as_the_model_sees_it_without_the_rendered_diagnostics() {
    let setup = chat_setup(Default::default());
    let result = ToolExecutionResult {
        diagnostics: Some(vec![ToolDiagnostic::new(DiagnosticSeverity::Info, "note")]),
        ..output(vec![text("a\nb\nc")])
    };
    let limits = ToolOutputLimits { max_lines: Some(1), ..Default::default() };
    add_tool(&setup, returning("long", result).with_output_limits(limits));
    let received = nested(&setup, "long").await;
    assert_eq!(value(&received), Some(json!("a\n")));
    assert_eq!(codes(&received), [None, Some("truncated".to_owned())]);
}

#[tokio::test]
async fn stores_exactly_what_callers_and_events_get_without_the_model_s_output_and_keeps_a_stored_null() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, returning("plain", output(vec![text("a")])));
    add_tool(
        &setup,
        returning("nothing", structured(Value::Null)).with_structured_output_schema(json!({ "type": "null" })),
    );
    let (tool, received) = probe(vec![("plain", json!({})), ("nothing", json!({}))]);
    add_tool(&setup, tool);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    let stream = watch_events(&harness, root.id, &ctx()).await.unwrap();
    let ends = Arc::new(Mutex::new(Vec::new()));
    let sink = ends.clone();
    stream
        .start(move |batch, _| {
            for event in batch {
                if event["type"] == "tool_execution_end".into() && event.get("parentToolCallId").is_some() {
                    sink.lock().unwrap().push(Value::from(&event["result"]));
                }
            }
            async { Ok(()) }
        })
        .unwrap();
    go(&root).await;
    harness.wait_for_idle(&ctx()).await.unwrap();
    stream.stop().await;
    harness.close(&ctx()).await.unwrap();

    let received: Vec<Value> =
        received.lock().unwrap().iter().map(|result| serde_json::to_value(result).unwrap()).collect();
    assert_eq!((received[0]["structuredOutput"].clone(), received[0]["isError"].clone()), (json!("a"), json!(false)));
    assert_eq!((received[1]["structuredOutput"].clone(), received[1]["isError"].clone()), (Value::Null, json!(false)));
    assert!(received[1].as_object().unwrap().contains_key("structuredOutput"));
    assert!(received[0].get("output").is_none());
    let stored: Vec<Value> = storage
        .commits()
        .into_iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::DocumentCreate { record, content: DocumentContent::Base { value, .. } }
                if record.kind == "pi.tool.nested-result" =>
            {
                Some(Value::from(&value["result"]))
            }
            _ => None,
        })
        .collect();
    assert_eq!(stored, received);
    assert_eq!(*ends.lock().unwrap(), received);
}

#[tokio::test]
async fn gives_callers_a_schema_tool_s_validated_structured_output_which_the_transcript_does_not_store() {
    let result =
        ToolExecutionResult { structured_output: Some(json!({ "count": 3 }).into()), ..output(vec![text("3 things")]) };
    let setup = chat_setup(Default::default());
    add_tool(&setup, returning("counter", result.clone()).with_structured_output_schema(count_schema()));
    let received = nested(&setup, "counter").await;
    assert_eq!((value(&received), received.is_error), (Some(json!({ "count": 3 })), false));

    let model_setup = chat_setup(Default::default());
    add_tool(&model_setup, returning("counter", result).with_structured_output_schema(count_schema()));
    let stored = direct(&model_setup, "counter").await;
    assert!(!stored.is_error);
    assert_eq!(stored.content, [text("3 things")]);
    assert!(serde_json::to_value(&stored).unwrap().get("structuredOutput").is_none());
}

#[tokio::test]
async fn turns_structured_output_that_breaks_the_schema_into_an_error_result_for_callers_and_a_report_for_the_model_s_calls()
 {
    let cases = [
        ("wrong", structured(json!({ "count": "three" })), true, "does not match its schema: count:"),
        ("missing", output(Vec::new()), true, "returned no structuredOutput"),
        ("undeclared", structured(json!({ "count": 3 })), false, "declares no structuredOutputSchema"),
    ];
    for (name, result, schema, message) in cases {
        let register = |setup: &ChatSetup| {
            let tool = returning(name, result.clone());
            add_tool(setup, if schema { tool.with_structured_output_schema(count_schema()) } else { tool });
        };
        let setup = chat_setup(Default::default());
        register(&setup);
        let received = nested(&setup, name).await;
        assert!(received.is_error);
        // The broken value is dropped; a schema-less tool's error result still carries its output, here none.
        assert_eq!(value(&received), (!schema).then(|| json!("")));
        assert_eq!(codes(&received), [Some("invalid_structured_output".to_owned())]);
        assert!(received.diagnostics[0].message.contains(message), "{}", received.diagnostics[0].message);

        let model_setup = chat_setup(Default::default());
        register(&model_setup);
        // The model never sees structured output: its call stands, and the host gets a report.
        let stored = direct(&model_setup, name).await;
        assert!(!stored.is_error);
        assert!(!serde_json::to_string(&stored.content).unwrap().contains(message));
        assert!(model_setup.reports().join("\n").contains(message), "{:?}", model_setup.reports());
    }
}

#[tokio::test]
async fn gives_callers_the_output_of_an_error_result_of_a_tool_without_a_schema() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, returning("bad", ToolExecutionResult::error("bad")));
    let received = nested(&setup, "bad").await;
    assert_eq!((received.is_error, value(&received)), (true, Some(json!("bad"))));
}

#[tokio::test]
async fn gives_callers_no_structured_output_for_an_error_result_the_harness_wrote_only_diagnostics() {
    let received = nested(&chat_setup(Default::default()), "missing").await;
    assert!(received.is_error);
    assert_eq!(codes(&received), [Some("tool_unavailable".to_owned())]);
    assert_eq!(value(&received), None);
}

#[tokio::test]
async fn lets_an_error_result_of_a_schema_tool_omit_structured_output_and_keeps_none_of_its_output() {
    let setup = chat_setup(Default::default());
    add_tool(
        &setup,
        returning("fails", ToolExecutionResult::error("nope")).with_structured_output_schema(count_schema()),
    );
    let received = nested(&setup, "fails").await;
    assert!(received.is_error && received.diagnostics.is_empty());
    assert_eq!(value(&received), None);
}

#[tokio::test]
async fn keeps_structured_output_when_after_tool_replaces_the_output_and_validates_one_after_tool_replaces() {
    let setup = chat_setup(Default::default());
    add_tool(&setup, returning("plain", output(vec![text("secret")])));
    add_tool(
        &setup,
        returning("counter", structured(json!({ "count": 1 }))).with_structured_output_schema(count_schema()),
    );
    add_tool(
        &setup,
        returning("other", structured(json!({ "count": 1 }))).with_structured_output_schema(count_schema()),
    );
    let hooks = ToolHooks {
        before_tool: None,
        after_tool: Some(Arc::new(|(call, result): (ToolHookCall, ToolExecutionResult), _, _| {
            let replaced = match call.call.name.as_str() {
                "other" => {
                    ToolExecutionResult { structured_output: Some(json!({ "count": "broken" }).into()), ..result }
                }
                "probe" => result,
                _ => ToolExecutionResult { output: Some(vec![text("rewritten")]), ..result },
            };
            Box::pin(async move { Ok(Some(replaced)) })
        })),
    };
    setup.registry.install(Extension::new("hooks").hook(hook(&tool_task(), hooks))).unwrap();
    let (tool, received) = probe(vec![("plain", json!({})), ("counter", json!({})), ("other", json!({}))]);
    add_tool(&setup, tool);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    go(&root).await;
    harness.close(&ctx()).await.unwrap();
    let received = received.lock().unwrap().clone();
    // A schema-less tool's value is its output, so redacting the output redacts what programs get.
    assert_eq!(value(&received[0]), Some(json!("rewritten")));
    assert_eq!(value(&received[1]), Some(json!({ "count": 1 })));
    assert!(received[2].is_error);
    assert_eq!(codes(&received[2]), [Some("invalid_structured_output".to_owned())]);
}

#[tokio::test]
async fn gives_callers_of_bash_its_kept_output_and_exit_code_also_for_a_nonzero_exit() {
    if cfg!(windows) {
        // The commands are POSIX shell; the Windows shell cases live in the tools tests.
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let setup = chat_setup(Default::default());
    add_tool(&setup, create_bash_tool(BashToolOptions::default()));
    let (tool, received) =
        probe(vec![("bash", json!({ "command": "printf ok" })), ("bash", json!({ "command": "printf bad; exit 4" }))]);
    add_tool(&setup, tool);
    setup.faux.set_responses([call("probe", "c1"), answer("done")]);
    let env = LocalExecutionEnv::new(LocalEnvOptions::new(directory.path().to_string_lossy().into_owned()));
    let options = setup.options().with_shared_env(Arc::new(env));
    let (harness, root) = open_chat_with(Arc::new(MemoryStorage::new()), &setup, options).await;
    go(&root).await;
    harness.close(&ctx()).await.unwrap();
    let received = received.lock().unwrap().clone();
    assert!(!received[0].is_error);
    assert_eq!(value(&received[0]), Some(json!({ "output": "ok", "truncated": false, "exitCode": 0 })));
    assert!(received[1].is_error);
    assert_eq!(value(&received[1]), Some(json!({ "output": "bad", "truncated": false, "exitCode": 4 })));
    assert_eq!(codes(&received[1]), [Some("exit_code".to_owned())]);
}
