//! Nested tool calls, ported from upstream `nested-tool-calls.test.ts`, plus the loop wiring.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::future::{BoxFuture, join_all};
use pi_agent::{
    AgentContext, AgentEvent, AgentTool, AgentToolCallOutcome, AgentToolResult, ExecuteToolOptions, NESTED_CALL_LIMITS,
    NestedCallRecorder, NestedToolCallHost, NestedToolCallRunner, SharedTool, ToolContext, ToolExecutionMode,
    ToolUpdateCallback, combine_usage, run_agent_loop,
};
use pi_ai::{Message, ToolCall, Usage};
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;

fn usage(input: u64, cost: f64) -> Usage {
    let mut usage = Usage { input, total_tokens: input, ..Usage::default() };
    usage.cost.input = cost;
    usage.cost.total = cost;
    usage
}

#[test]
fn combined_totals_are_provider_reported_only_when_both_parts_are() {
    let reported = |total: f64, from_provider: Option<bool>| {
        let mut usage = usage(1, total);
        usage.cost.total_from_provider = from_provider;
        usage
    };
    let both = combine_usage(&reported(1.0, Some(true)), &reported(2.0, Some(true)));
    assert_eq!((both.cost.total, both.cost.total_from_provider), (3.0, Some(true)));
    for (first, second) in [(Some(true), None), (None, Some(true)), (None, None), (Some(false), Some(true))] {
        let combined = combine_usage(&reported(1.0, first), &reported(2.0, second));
        assert_eq!(combined.cost.total_from_provider, Some(false), "{first:?} + {second:?}");
    }
}

type Body = Arc<dyn Fn(ToolContext) -> BoxFuture<'static, AgentToolResult> + Send + Sync>;

/// A tool whose body receives the call's context.
struct CtxTool {
    name: String,
    mode: Option<ToolExecutionMode>,
    body: Body,
}

fn ctx_tool(
    name: &str,
    body: impl Fn(ToolContext) -> BoxFuture<'static, AgentToolResult> + Send + Sync + 'static,
) -> SharedTool {
    Arc::new(CtxTool { name: name.into(), mode: None, body: Arc::new(body) })
}

#[async_trait]
impl AgentTool for CtxTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "test"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object"})
    }
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.mode
    }
    async fn execute(
        &self,
        id: &str,
        _: Value,
        _: CancellationToken,
        _: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        Ok((self.body)(ToolContext::new(id)).await)
    }
    async fn execute_with_context(
        &self,
        context: &ToolContext,
        _: Value,
        _: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        on_update(AgentToolResult::text("partial"));
        Ok((self.body)(context.clone()).await)
    }
}

/// Resolves calls against a mutable tool list without hooks, like the upstream test host.
struct TestHost {
    tools: Mutex<Vec<SharedTool>>,
    sequential: bool,
    events: Mutex<Vec<(String, String, Option<String>)>>,
}

#[async_trait]
impl NestedToolCallHost for TestHost {
    fn tools(&self) -> Vec<SharedTool> {
        self.tools.lock().unwrap().clone()
    }
    fn is_sequential(&self) -> bool {
        self.sequential
    }
    async fn run_tool_call(
        &self,
        call: ToolCall,
        _parent: &str,
        context: ToolContext,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> AgentToolCallOutcome {
        let Some(tool) = self.tools().into_iter().find(|tool| tool.name() == call.name) else {
            let result = AgentToolResult::error(format!("Tool {} not found", call.name));
            return AgentToolCallOutcome { tool_call: call, result, is_error: true, duration_ms: None };
        };
        let args = Value::Object(call.arguments.clone());
        let result = tool.execute_with_context(&context, args, cancel, on_update).await.unwrap();
        AgentToolCallOutcome { is_error: result.is_error, tool_call: call, result, duration_ms: Some(0) }
    }
    async fn emit(&self, event: AgentEvent) {
        let value = serde_json::to_value(&event).unwrap();
        let text = |key: &str| value[key].as_str().map(String::from);
        self.events.lock().unwrap().push((
            text("type").unwrap(),
            text("toolCallId").unwrap(),
            text("parentToolCallId"),
        ));
    }
}

fn runner(tools: Vec<SharedTool>, sequential: bool) -> (Arc<TestHost>, Arc<NestedToolCallRunner>) {
    let host = Arc::new(TestHost { tools: Mutex::new(tools), sequential, events: Mutex::default() });
    (host.clone(), NestedToolCallRunner::new(host))
}

fn context(runner: &Arc<NestedToolCallRunner>, id: &str) -> ToolContext {
    ToolContext::nested(id, runner.clone(), CancellationToken::new())
}

fn plain(result: AgentToolResult) -> impl Fn(ToolContext) -> BoxFuture<'static, AgentToolResult> + Send + Sync {
    move |_| {
        let result = result.clone();
        async move { result }.boxed()
    }
}

#[tokio::test]
async fn assigns_ids_below_the_caller_emits_parent_ids_and_records_calls() {
    let (host, runner) = runner(vec![ctx_tool("echo", plain(AgentToolResult::text("ok")))], false);
    let updates = Arc::new(AtomicUsize::new(0));
    let seen = updates.clone();
    let options = ExecuteToolOptions {
        on_update: Some(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
        })),
        ..Default::default()
    };
    let caller = context(&runner, "call");
    let first = caller.execute_tool("echo", json!({"a": 1}), options).await;
    let missing = caller.execute_tool("missing", json!({}), ExecuteToolOptions::default()).await;
    assert_eq!(first.tool_call.id, "call/1");
    assert!(missing.is_error && missing.tool_call.id == "call/2");
    assert_eq!(updates.load(Ordering::SeqCst), 1);
    let events: Vec<(String, String, Option<String>)> = host.events.lock().unwrap().clone();
    let expected = [
        ("tool_execution_start", "call/1"),
        ("tool_execution_update", "call/1"),
        ("tool_execution_end", "call/1"),
        ("tool_execution_start", "call/2"),
        ("tool_execution_end", "call/2"),
    ];
    let expected: Vec<_> =
        expected.iter().map(|(kind, id)| (kind.to_string(), id.to_string(), Some("call".to_string()))).collect();
    assert_eq!(events, expected);
    let record = runner.take_record("call").unwrap().calls.unwrap();
    assert!(record.complete);
    assert_eq!(record.calls[0].arguments, json!({"a": 1}).as_object().cloned());
    assert_eq!((record.calls[0].status.as_str(), record.calls[1].status.as_str()), ("ok", "error"));
    assert_eq!(record.calls[1].error.as_deref(), Some("Tool missing not found"));
    assert!(record.calls.iter().all(|call| call.duration_ms.is_some()));
    // The record is taken once.
    assert!(runner.take_record("call").is_none() && runner.take_record("other").is_none());
}

#[tokio::test]
async fn records_calls_of_nested_tools_on_the_model_issued_call() {
    let middle = ctx_tool("middle", |context| {
        async move {
            context.execute_tool("leaf", json!({}), ExecuteToolOptions::default()).await;
            AgentToolResult::default()
        }
        .boxed()
    });
    let (_, runner) = runner(vec![ctx_tool("leaf", plain(AgentToolResult::default())), middle], false);
    context(&runner, "call").execute_tool("middle", json!({}), ExecuteToolOptions::default()).await;
    let ids: Vec<String> =
        runner.take_record("call").unwrap().calls.unwrap().calls.into_iter().map(|call| call.id).collect();
    assert_eq!(ids, ["call/1", "call/1/1"]);
}

#[tokio::test]
async fn sums_the_usage_of_nested_results_at_every_depth() {
    let middle = ctx_tool("middle", |context| {
        async move {
            context.execute_tool("leaf", json!({}), ExecuteToolOptions::default()).await;
            // Its own usage only: the leaf's usage is counted once, by the recorder.
            AgentToolResult::default().with_usage(usage(5, 0.005))
        }
        .boxed()
    });
    let leaf = ctx_tool("leaf", plain(AgentToolResult::default().with_usage(usage(10, 0.01))));
    let (_, runner) = runner(vec![leaf, ctx_tool("plain", plain(AgentToolResult::default())), middle], false);
    let (call, free) = (context(&runner, "call"), context(&runner, "free"));
    for name in ["middle", "leaf", "plain"] {
        call.execute_tool(name, json!({}), ExecuteToolOptions::default()).await;
    }
    free.execute_tool("plain", json!({}), ExecuteToolOptions::default()).await;
    let summary = runner.take_record("call").unwrap();
    let total = summary.usage.unwrap();
    assert_eq!(total.input, 25);
    assert!((total.cost.total - 0.025).abs() < 1e-10);
    let free = runner.take_record("free").unwrap();
    assert!(free.calls.unwrap().complete && free.usage.is_none());
}

#[tokio::test]
async fn serializes_concurrent_calls_to_sequential_tools() {
    let active = Arc::new(AtomicUsize::new(0));
    let peaks = Arc::new(Mutex::new([0usize; 2]));
    let make = |name: &str, slot: usize, mode: Option<ToolExecutionMode>| -> SharedTool {
        let (active, peaks) = (active.clone(), peaks.clone());
        let body: Body = Arc::new(move |_| {
            let (active, peaks) = (active.clone(), peaks.clone());
            async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                {
                    let mut peak = peaks.lock().unwrap();
                    peak[slot] = peak[slot].max(now);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                AgentToolResult::default()
            }
            .boxed()
        });
        Arc::new(CtxTool { name: name.into(), mode, body })
    };
    let tools = vec![make("sequential", 0, Some(ToolExecutionMode::Sequential)), make("parallel", 1, None)];
    let (_, runner) = runner(tools, false);
    let caller = context(&runner, "call");
    join_all((0..3).map(|_| caller.execute_tool("sequential", json!({}), ExecuteToolOptions::default()))).await;
    join_all((0..3).map(|_| caller.execute_tool("parallel", json!({}), ExecuteToolOptions::default()))).await;
    assert_eq!(*peaks.lock().unwrap(), [1, 3]);
}

fn recorded_call(id: &str, args: Value) -> ToolCall {
    ToolCall { id: id.into(), name: "t".into(), arguments: args.as_object().cloned().unwrap(), ..Default::default() }
}

#[test]
fn recorder_omits_oversized_arguments_and_drops_calls_beyond_the_limit() {
    let mut recorder = NestedCallRecorder::default();
    assert!(recorder.snapshot().is_none());
    let small = recorder.start(&recorded_call("a", json!({"x": 1})));
    recorder.finish(small, false, "");
    let snapshot = recorder.snapshot().unwrap();
    assert!(snapshot.complete && snapshot.calls[0].status == "ok" && snapshot.calls[0].arguments.is_some());

    let big = recorder
        .start(&recorded_call("b", json!({"text": "x".repeat(NESTED_CALL_LIMITS.max_argument_bytes_per_call)})));
    recorder.finish(big, true, &"e".repeat(1000));
    let snapshot = recorder.snapshot().unwrap();
    assert!(!snapshot.complete);
    let call = &snapshot.calls[1];
    assert_eq!((call.id.as_str(), call.status.as_str(), call.arguments.is_none()), ("b", "error", true));
    assert!(call.arguments_bytes.unwrap() > NESTED_CALL_LIMITS.max_argument_bytes_per_call as u64);
    assert_eq!(call.error.as_ref().unwrap().len(), NESTED_CALL_LIMITS.max_error_chars);

    for index in 0..NESTED_CALL_LIMITS.max_calls {
        let started = recorder.start(&recorded_call(&format!("c{index}"), json!({})));
        recorder.finish(started, false, "");
    }
    assert_eq!(recorder.snapshot().unwrap().calls.len(), NESTED_CALL_LIMITS.max_calls);
}

#[test]
fn recorder_caps_the_total_argument_size_and_marks_unfinished_calls_incomplete() {
    let mut recorder = NestedCallRecorder::default();
    let chunk = json!({"text": "x".repeat(7000)});
    let started: Vec<_> =
        (0..6).map(|index| recorder.start(&recorded_call(&format!("c{index}"), chunk.clone()))).collect();
    let snapshot = recorder.snapshot().unwrap();
    // 32 KiB fits four 7000-byte argument objects.
    assert_eq!(snapshot.calls.iter().filter(|call| call.arguments.is_some()).count(), 4);
    assert!(snapshot.calls.iter().all(|call| call.status == "unfinished"));
    assert!(!snapshot.complete);
    assert_eq!(started.len(), 6);
}

#[tokio::test]
async fn the_loop_runs_nested_calls_with_hooks_and_records_them_on_the_result() {
    let orchestrate = ctx_tool("orchestrate", |context| {
        async move {
            let ok = context.execute_tool("echo", json!({"value": "inner"}), ExecuteToolOptions::default()).await;
            let invalid = context.execute_tool("echo", json!({}), ExecuteToolOptions::default()).await;
            let blocked =
                context.execute_tool("echo", json!({"value": "blocked"}), ExecuteToolOptions::default()).await;
            let model = context.model().map(|model| model.id.clone()).unwrap_or_default();
            let text = format!(
                "{} {} {} {model}",
                ok.result.content[0].as_text().unwrap(),
                invalid.is_error,
                blocked.is_error
            );
            AgentToolResult::text(text).with_usage(usage(1, 0.0))
        }
        .boxed()
    });
    let echo = FnTool::new(
        "echo",
        json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}),
        |_, args| Ok(AgentToolResult::text(args["value"].as_str().unwrap()).with_usage(usage(10, 0.0))),
    );
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("model-1", "orchestrate", json!({}))])]);
    let mut config = config(stream_fn);
    let parents = Arc::new(Mutex::new(Vec::new()));
    let seen = parents.clone();
    config.before_tool_call = Some(Arc::new(move |input, _| {
        seen.lock().unwrap().push(input.parent_tool_call_id.map(String::from));
        let block = input.args["value"] == "blocked";
        async move {
            block.then(|| pi_agent::BeforeToolCallResult {
                block: true,
                reason: Some("no".into()),
                terminate: false,
                args: None,
            })
        }
        .boxed()
    }));
    let mut context =
        AgentContext { system_prompt: Some(String::new()), tools: vec![orchestrate, echo], ..Default::default() };
    let (events, sink) = recorder();
    let added = run_agent_loop(vec![Message::user("go")], &mut context, &config, sink, CancellationToken::new()).await;

    let Some(Message::ToolResult(result)) = added.iter().find(|message| matches!(message, Message::ToolResult(_)))
    else {
        panic!("expected a tool result");
    };
    let expected = format!("inner true true {}", config.model.id);
    assert_eq!(result.content[0].as_text(), Some(expected.as_str()));
    let nested = result.nested_calls.as_ref().unwrap();
    let ids: Vec<&str> = nested.calls.iter().map(|call| call.id.as_str()).collect();
    assert_eq!(ids, ["model-1/1", "model-1/2", "model-1/3"]);
    assert_eq!(result.usage.unwrap().input, 11);
    // The model call, then the three nested calls (validation failures skip the hook).
    assert_eq!(*parents.lock().unwrap(), [None, Some("model-1".to_string()), Some("model-1".to_string())]);
    let nested_events = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| matches!(event, AgentEvent::ToolExecutionStart { parent_tool_call_id: Some(_), .. }))
        .count();
    assert_eq!(nested_events, 3);
    assert!(context.tools.len() == 2, "the context comes back after the batch");
}

#[tokio::test]
async fn a_nested_call_runs_the_tool_its_hook_saw_when_callable_tools_change_during_the_hook() {
    let orchestrate = ctx_tool("orchestrate", |context| {
        async move {
            let outcome = context.execute_tool("echo", json!({}), ExecuteToolOptions::default()).await;
            AgentToolResult::text(outcome.result.content[0].as_text().unwrap_or_default())
        }
        .boxed()
    });
    let echo = |reply: &'static str| -> SharedTool {
        let tool: SharedTool =
            FnTool::new("echo", json!({"type": "object"}), move |_, _| Ok(AgentToolResult::text(reply)));
        tool
    };
    let (original, replacement) = (echo("original"), echo("replacement"));
    let callable = Arc::new(Mutex::new(vec![orchestrate.clone(), original.clone()]));
    let (_, stream_fn) = sequence(vec![tool_use(vec![call("model-1", "orchestrate", json!({}))])]);
    let mut config = config(stream_fn);
    let listed = callable.clone();
    config.callable_tools = Some(Arc::new(move || listed.lock().unwrap().clone()));
    let hooked = Arc::new(Mutex::new(Vec::<SharedTool>::new()));
    let (seen, swap, swapped) = (hooked.clone(), callable.clone(), vec![orchestrate.clone(), replacement.clone()]);
    config.before_tool_call = Some(Arc::new(move |input, _| {
        seen.lock().unwrap().push(input.tool.clone());
        if input.parent_tool_call_id.is_some() {
            *swap.lock().unwrap() = swapped.clone();
        }
        async { None }.boxed()
    }));
    let mut context =
        AgentContext { system_prompt: Some(String::new()), tools: vec![orchestrate.clone()], ..Default::default() };
    let (_, sink) = recorder();
    let added = run_agent_loop(vec![Message::user("go")], &mut context, &config, sink, CancellationToken::new()).await;

    let Some(Message::ToolResult(result)) = added.iter().find(|message| matches!(message, Message::ToolResult(_)))
    else {
        panic!("expected a tool result");
    };
    assert_eq!(result.content[0].as_text(), Some("original"));
    let same = |a: &SharedTool, b: &SharedTool| std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b));
    let hooked = hooked.lock().unwrap();
    assert!(hooked.len() == 2 && same(&hooked[0], &orchestrate) && same(&hooked[1], &original));
    // A lookup by name after the hook finds the replacement, which never ran.
    assert!(same(&callable.lock().unwrap()[1], &replacement));
}

#[tokio::test]
async fn tools_outside_the_loop_cannot_run_nested_calls() {
    let outcome = ToolContext::new("solo").execute_tool("echo", json!({}), ExecuteToolOptions::default()).await;
    assert!(outcome.is_error && !ToolContext::new("solo").can_execute_tools());
    assert!(ToolContext::new("solo").model().is_none());
}
