//! `run_tool_call`, `prepare_arguments`, and argument replacement, ported from upstream
//! `agent-loop.test.ts`.

mod support;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::FutureExt;
use pi_agent::{
    AfterToolCallFn, AfterToolCallResult, AgentContext, AgentTool, AgentToolResult, BeforeToolCallFn,
    BeforeToolCallResult, RunToolCallOptions, SharedTool, ToolUpdateCallback, run_agent_loop, run_tool_call,
};
use pi_ai::{ContentBlock, Message, ToolCall};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

/// `edit` that accepts legacy top-level `oldText`/`newText` through `prepare_arguments`.
struct LegacyEdit {
    executed: Mutex<Vec<Value>>,
}

#[async_trait]
impl AgentTool for LegacyEdit {
    fn name(&self) -> &str {
        "edit"
    }
    fn description(&self) -> &str {
        "Edit tool"
    }
    fn parameters(&self) -> Value {
        let replace = json!({"type": "object", "properties": {"oldText": {"type": "string"}, "newText": {"type": "string"}}, "required": ["oldText", "newText"]});
        json!({"type": "object", "properties": {"edits": {"type": "array", "items": replace}}, "required": ["edits"]})
    }
    fn prepare_arguments(&self, args: Value) -> anyhow::Result<Value> {
        let (Some(old), Some(new)) =
            (args.get("oldText").and_then(Value::as_str), args.get("newText").and_then(Value::as_str))
        else {
            return Ok(args);
        };
        let mut edits = args.get("edits").and_then(Value::as_array).cloned().unwrap_or_default();
        edits.push(json!({"oldText": old, "newText": new}));
        Ok(json!({ "edits": edits }))
    }
    async fn execute(
        &self,
        _: &str,
        args: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.executed.lock().unwrap().push(args["edits"].clone());
        Ok(AgentToolResult::text("edited"))
    }
}

#[tokio::test]
async fn prepares_tool_arguments_before_validation() {
    let tool = Arc::new(LegacyEdit { executed: Mutex::default() });
    let (_, stream_fn) =
        sequence(vec![tool_use(vec![call("tool-1", "edit", json!({"oldText": "before", "newText": "after"}))])]);
    let mut context =
        AgentContext { system_prompt: Some(String::new()), tools: vec![tool.clone()], ..Default::default() };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("edit")], &mut context, &config(stream_fn), sink, CancellationToken::new()).await;
    assert_eq!(*tool.executed.lock().unwrap(), [json!([{"oldText": "before", "newText": "after"}])]);
}

#[tokio::test]
async fn executes_arguments_a_before_hook_replaced_without_revalidation() {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let seen = executed.clone();
    let echo = FnTool::new(
        "echo",
        json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}),
        move |_, args| {
            seen.lock().unwrap().push(args["value"].clone());
            Ok(AgentToolResult::text("ok"))
        },
    );
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("tool-1", "echo", json!({"value": "hello"}))])]);
    let mut config = config(stream_fn);
    config.before_tool_call = Some(Arc::new(|_, _| {
        async { Some(BeforeToolCallResult { args: Some(json!({"value": 123})), ..Default::default() }) }.boxed()
    }));
    let mut context = AgentContext { system_prompt: Some(String::new()), tools: vec![echo], ..Default::default() };
    let (_, sink) = recorder();
    run_agent_loop(vec![Message::user("go")], &mut context, &config, sink, CancellationToken::new()).await;
    assert_eq!(*executed.lock().unwrap(), [json!(123)]);
}

fn echo_tool() -> SharedTool {
    FnTool::new(
        "echo",
        json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}),
        |_, args| {
            let value = args["value"].as_str().unwrap_or_default().to_string();
            Ok(AgentToolResult::text(value.clone()).with_structured_content(json!({ "value": value })))
        },
    )
}

fn failing_tool() -> SharedTool {
    FnTool::new("failing", json!({"type": "object"}), |_, _| {
        Ok(AgentToolResult { is_error: true, ..AgentToolResult::text("bad") }.with_details(json!({"partial": true})))
    })
}

fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.as_object().cloned().unwrap(), ..Default::default() }
}

#[tokio::test]
async fn run_tool_call_validates_runs_hooks_and_reports_failures_as_errors() {
    let hook_calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = hook_calls.clone();
    let before: BeforeToolCallFn = Arc::new(move |input, _| {
        seen.lock().unwrap().push(format!("before {}", input.tool_call.id));
        let blocked = input.args["value"] == "blocked";
        async move {
            blocked.then(|| BeforeToolCallResult { block: true, reason: Some("nope".into()), ..Default::default() })
        }
        .boxed()
    });
    let seen = hook_calls.clone();
    let after: AfterToolCallFn = Arc::new(move |input, _| {
        seen.lock().unwrap().push(format!("after {}", input.tool_call.id));
        async { None }.boxed()
    });
    let tools = vec![echo_tool(), failing_tool()];
    let message = text("");
    let context = AgentContext::default();
    let run = |call: ToolCall| {
        let options = RunToolCallOptions {
            tools: &tools,
            assistant_message: &message,
            context: &context,
            before_tool_call: Some(&before),
            after_tool_call: Some(&after),
            parent_tool_call_id: None,
            tool_context: None,
            cancel: CancellationToken::new(),
            on_update: None,
        };
        run_tool_call(call, options)
    };
    let ok = run(tool_call("a", "echo", json!({"value": "a"}))).await;
    assert!(!ok.is_error && ok.tool_call.id == "a");
    assert_eq!(ok.result.structured_content, Some(json!({"value": "a"})));
    assert!(run(tool_call("b", "echo", json!({"value": {"nested": true}}))).await.is_error);
    let blocked = run(tool_call("c", "echo", json!({"value": "blocked"}))).await;
    assert!(blocked.is_error && blocked.result.content[0].as_text() == Some("nope"));
    let missing = run(tool_call("d", "missing", json!({}))).await;
    assert!(missing.is_error && missing.result.content[0].as_text() == Some("Tool missing not found"));
    // Error results keep their details.
    let failed = run(tool_call("e", "failing", json!({}))).await;
    assert!(failed.is_error && failed.result.details == Some(json!({"partial": true})));
    // Validation failures and unknown tools never reach the hooks; blocked calls skip `after`.
    assert_eq!(*hook_calls.lock().unwrap(), ["before a", "after a", "before c", "before e", "after e"]);
}

#[tokio::test]
async fn after_hooks_replace_structured_content_and_drop_it_with_replaced_content() {
    let redacted = vec![ContentBlock::text("redacted")];
    let results = [
        AfterToolCallResult { content: Some(redacted.clone()), ..Default::default() },
        AfterToolCallResult { structured_content: Some(json!({"value": "replaced"})), ..Default::default() },
        AfterToolCallResult {
            content: Some(redacted),
            structured_content: Some(json!({"value": "both"})),
            ..Default::default()
        },
        AfterToolCallResult { details: Some(json!({"note": "kept"})), ..Default::default() },
    ];
    let tools = vec![echo_tool()];
    let (message, context) = (text(""), AgentContext::default());
    let mut seen = Vec::new();
    for update in results {
        let after: AfterToolCallFn = Arc::new(move |_, _| {
            let update = update.clone();
            async move { Some(update) }.boxed()
        });
        let options = RunToolCallOptions {
            tools: &tools,
            assistant_message: &message,
            context: &context,
            before_tool_call: None,
            after_tool_call: Some(&after),
            parent_tool_call_id: None,
            tool_context: None,
            cancel: CancellationToken::new(),
            on_update: None,
        };
        seen.push(
            run_tool_call(tool_call("x", "echo", json!({"value": "original"})), options)
                .await
                .result
                .structured_content,
        );
    }
    assert_eq!(
        seen,
        [None, Some(json!({"value": "replaced"})), Some(json!({"value": "both"})), Some(json!({"value": "original"}))]
    );
}
