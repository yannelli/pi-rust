use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use pi_agent::{
    Agent, AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentTool, AgentToolResult, BeforeToolCallResult,
    EventSink, StreamFn, ToolExecutionMode, ToolUpdateCallback, run_agent_loop,
};
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::{ApiProvider, Message, StopReason};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct EchoTool {
    calls: AtomicUsize,
    delay: Duration,
    sequential: bool,
}

impl EchoTool {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self { calls: AtomicUsize::new(0), delay, sequential: false })
    }
}

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echo the text argument."
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"text": {"type": "string"}, "times": {"type": "integer"}}, "required": ["text"]})
    }

    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        self.sequential.then_some(ToolExecutionMode::Sequential)
    }

    async fn execute(
        &self,
        _id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        on_update(AgentToolResult::text("working"));
        tokio::select! {
            _ = tokio::time::sleep(self.delay) => {}
            _ = cancel.cancelled() => anyhow::bail!("cancelled"),
        }
        let times = args["times"].as_u64().unwrap_or(1) as usize;
        Ok(AgentToolResult::text(args["text"].as_str().unwrap_or_default().repeat(times)).with_details(args))
    }
}

fn faux(responses: Vec<FauxResponse>) -> (Arc<FauxProvider>, StreamFn) {
    let provider = FauxProvider::new(responses);
    let stream_provider = provider.clone();
    let stream_fn: StreamFn = Arc::new(move |model, context, options| stream_provider.stream(model, context, options));
    (provider, stream_fn)
}

fn recorder() -> (Arc<Mutex<Vec<AgentEvent>>>, EventSink) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    (events, Arc::new(move |event| sink_events.lock().unwrap().push(event)))
}

fn kinds(events: &[AgentEvent]) -> Vec<String> {
    events.iter().map(|event| serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

fn config(stream_fn: StreamFn) -> AgentLoopConfig {
    AgentLoopConfig { stream_fn: Some(stream_fn), ..AgentLoopConfig::new(FauxProvider::model()) }
}

#[tokio::test]
async fn runs_tool_calls_and_feeds_results_back() {
    let (provider, stream_fn) = faux(vec![
        FauxResponse::tool_call("c1", "echo", json!({"text": "hi", "times": "2"})),
        FauxResponse::text("done"),
    ]);
    let tool = EchoTool::new(Duration::ZERO);
    let mut context =
        AgentContext { system_prompt: Some("system".into()), messages: vec![], tools: vec![tool.clone()] };
    let (events, sink) = recorder();
    let added =
        run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, CancellationToken::new())
            .await;

    assert_eq!(added.len(), 4);
    assert_eq!(context.messages.len(), 4);
    let Message::ToolResult(result) = &added[2] else { panic!("expected tool result") };
    assert_eq!(result.content[0].as_text(), Some("hihi"));
    assert!(!result.is_error);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].system_prompt.as_deref(), Some("system"));
    assert_eq!(requests[0].tools[0].name, "echo");
    assert!(matches!(requests[1].messages[2], Message::ToolResult(_)));
    let kinds = kinds(&events.lock().unwrap());
    assert_eq!(kinds.first().map(String::as_str), Some("agent_start"));
    assert_eq!(kinds.last().map(String::as_str), Some("agent_end"));
    for expected in
        ["tool_execution_start", "tool_execution_update", "tool_execution_end", "message_update", "turn_end"]
    {
        assert!(kinds.iter().any(|kind| kind == expected), "missing {expected}: {kinds:?}");
    }
    assert_eq!(kinds.iter().filter(|kind| *kind == "turn_start").count(), 2);
}

#[tokio::test]
async fn reports_validation_errors_and_unknown_tools_without_executing() {
    let (_, stream_fn) = faux(vec![
        FauxResponse::tool_calls(&[("c1", "echo", json!({"times": 1})), ("c2", "missing", json!({}))]),
        FauxResponse::text("ok"),
    ]);
    let tool = EchoTool::new(Duration::ZERO);
    let mut context = AgentContext { tools: vec![tool.clone()], ..AgentContext::default() };
    let (_, sink) = recorder();
    let added =
        run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, CancellationToken::new())
            .await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    let results: Vec<_> = added
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        results[0].is_error && results[0].content[0].as_text().unwrap().contains("Validation failed for tool \"echo\"")
    );
    assert_eq!(results[1].content[0].as_text(), Some("Tool missing not found"));
}

#[tokio::test]
async fn parallel_tools_overlap_and_keep_call_order() {
    let calls =
        [("a", "echo", json!({"text": "a"})), ("b", "echo", json!({"text": "b"})), ("c", "echo", json!({"text": "c"}))];
    let (_, stream_fn) = faux(vec![FauxResponse::tool_calls(&calls), FauxResponse::text("ok")]);
    let tool = EchoTool::new(Duration::from_millis(200));
    let mut context = AgentContext { tools: vec![tool], ..AgentContext::default() };
    let (_, sink) = recorder();
    let started = std::time::Instant::now();
    let added =
        run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, CancellationToken::new())
            .await;
    assert!(started.elapsed() < Duration::from_millis(500), "tools did not run in parallel");
    let order: Vec<_> = added
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result.tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["a", "b", "c"]);
}

#[tokio::test]
async fn sequential_mode_runs_one_tool_at_a_time() {
    let calls = [("a", "echo", json!({"text": "a"})), ("b", "echo", json!({"text": "b"}))];
    let (_, stream_fn) = faux(vec![FauxResponse::tool_calls(&calls), FauxResponse::text("ok")]);
    let tool = Arc::new(EchoTool { calls: AtomicUsize::new(0), delay: Duration::from_millis(150), sequential: true });
    let mut context = AgentContext { tools: vec![tool], ..AgentContext::default() };
    let (_, sink) = recorder();
    let started = std::time::Instant::now();
    run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, CancellationToken::new()).await;
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[tokio::test]
async fn hooks_block_and_rewrite_results() {
    let (_, stream_fn) = faux(vec![
        FauxResponse::tool_calls(&[("a", "echo", json!({"text": "keep"})), ("b", "echo", json!({"text": "block"}))]),
        FauxResponse::text("ok"),
    ]);
    let tool = EchoTool::new(Duration::ZERO);
    let mut context = AgentContext { tools: vec![tool.clone()], ..AgentContext::default() };
    let mut config = config(stream_fn);
    config.before_tool_call = Some(Arc::new(|input, _| {
        let block = input.args["text"] == "block";
        async move { block.then(|| BeforeToolCallResult { block: true, reason: Some("denied".into()), terminate: false }) }.boxed()
    }));
    config.after_tool_call = Some(Arc::new(|input, _| {
        let text = format!("{}!", input.result.content[0].as_text().unwrap_or_default());
        async move {
            Some(pi_agent::AfterToolCallResult {
                content: Some(vec![pi_ai::ContentBlock::text(text)]),
                ..Default::default()
            })
        }
        .boxed()
    }));
    let (_, sink) = recorder();
    let added = run_agent_loop(vec![Message::user("go")], &mut context, &config, sink, CancellationToken::new()).await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    let texts: Vec<_> = added
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some((result.content[0].as_text().unwrap().to_string(), result.is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(texts, [("keep!".to_string(), false), ("denied".to_string(), true)]);
}

#[tokio::test]
async fn truncated_responses_do_not_execute_tool_calls() {
    let mut truncated = FauxResponse::tool_call("a", "echo", json!({"text": "x"}));
    if let FauxResponse::Message { stop, .. } = &mut truncated {
        *stop = StopReason::Length;
    }
    let (_, stream_fn) = faux(vec![truncated, FauxResponse::text("ok")]);
    let tool = EchoTool::new(Duration::ZERO);
    let mut context = AgentContext { tools: vec![tool.clone()], ..AgentContext::default() };
    let (_, sink) = recorder();
    let added =
        run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, CancellationToken::new())
            .await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert!(
        matches!(&added[2], Message::ToolResult(result) if result.is_error && result.content[0].as_text().unwrap().contains("output token limit"))
    );
}

#[tokio::test]
async fn cancellation_ends_the_run_as_aborted() {
    let (_, stream_fn) = faux(vec![FauxResponse::Hang]);
    let mut context = AgentContext::default();
    let (events, sink) = recorder();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let added = run_agent_loop(vec![Message::user("go")], &mut context, &config(stream_fn), sink, cancel).await;
    assert!(matches!(added.last(), Some(Message::Assistant(message)) if message.stop_reason == StopReason::Aborted));
    assert_eq!(kinds(&events.lock().unwrap()).last().map(String::as_str), Some("agent_end"));
}

#[tokio::test]
async fn agent_steering_and_follow_up_queues() {
    let (provider, stream_fn) = faux(vec![
        FauxResponse::tool_call("a", "echo", json!({"text": "x"})),
        FauxResponse::text("after steering"),
        FauxResponse::text("after follow-up"),
    ]);
    let agent = Agent::new(FauxProvider::model());
    agent.update_state(|state| state.tools = vec![EchoTool::new(Duration::from_millis(100))]);
    agent.set_hooks(AgentHooks { stream_fn: Some(stream_fn), ..AgentHooks::default() });
    let steering_agent = agent.clone();
    agent.subscribe(move |event| {
        if matches!(event, AgentEvent::ToolExecutionStart { .. }) {
            steering_agent.steer(Message::user("steer"));
            steering_agent.follow_up(Message::user("follow"));
        }
    });
    agent.prompt("go", vec![]).await.unwrap();
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let last_user = |index: usize| match requests[index]
        .messages
        .iter()
        .rev()
        .find(|message| matches!(message, Message::User(_)))
    {
        Some(Message::User(user)) => user.content.text(),
        _ => String::new(),
    };
    assert_eq!(last_user(1), "steer");
    assert_eq!(last_user(2), "follow");
    let state = agent.state();
    assert!(!state.is_streaming && !agent.is_running());
    assert_eq!(state.messages.len(), 7);
}

#[tokio::test]
async fn agent_rejects_concurrent_prompts_and_records_errors() {
    let (_, stream_fn) = faux(vec![FauxResponse::Hang, FauxResponse::Error("boom".into())]);
    let agent = Agent::new(FauxProvider::model());
    agent.set_hooks(AgentHooks { stream_fn: Some(stream_fn), ..AgentHooks::default() });
    let running = agent.clone();
    let run = tokio::spawn(async move { running.prompt("first", vec![]).await });
    while !agent.is_running() {
        tokio::task::yield_now().await;
    }
    assert!(agent.prompt("second", vec![]).await.is_err());
    agent.abort();
    run.await.unwrap().unwrap();
    agent.wait_for_idle().await;
    agent.prompt("third", vec![]).await.unwrap();
    assert_eq!(agent.state().error_message.as_deref(), Some("boom"));
}
