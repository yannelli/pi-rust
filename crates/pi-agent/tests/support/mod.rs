//! Shared helpers for the agent loop tests: a scripted stream function, event
//! recording, and small tools.
#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_agent::{AgentEvent, AgentLoopConfig, AgentTool, AgentToolResult, EventSink, StreamFn, ToolUpdateCallback};
use pi_ai::stream::event_channel;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, StopReason, StreamOptions,
    ThinkingLevel, ToolCall, Usage,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

pub fn model() -> Model {
    pi_ai::providers::faux::FauxProvider::model()
}

pub fn assistant(content: Vec<ContentBlock>, stop: StopReason) -> AssistantMessage {
    AssistantMessage { content, stop_reason: stop, usage: Usage::default(), ..AssistantMessage::empty(&model()) }
}

pub fn text(text: &str) -> AssistantMessage {
    assistant(vec![ContentBlock::text(text)], StopReason::Stop)
}

pub fn call(id: &str, name: &str, arguments: Value) -> ContentBlock {
    ContentBlock::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    })
}

pub fn tool_use(calls: Vec<ContentBlock>) -> AssistantMessage {
    assistant(calls, StopReason::ToolUse)
}

/// One provider request as the stream function saw it.
#[derive(Clone)]
pub struct Request {
    pub model: Model,
    pub context: Context,
    pub reasoning: Option<ThinkingLevel>,
    pub session_id: Option<String>,
    pub transport: Option<pi_ai::Transport>,
    pub max_tokens: Option<u64>,
}

pub type Responder = Arc<dyn Fn(usize, &Context) -> AssistantMessage + Send + Sync>;

/// A stream function answering request `n` (from 0) with `respond(n, context)`.
pub fn scripted(
    respond: impl Fn(usize, &Context) -> AssistantMessage + Send + Sync + 'static,
) -> (Arc<Mutex<Vec<Request>>>, StreamFn) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let respond: Responder = Arc::new(respond);
    let stream_fn: StreamFn = Arc::new(move |model: &Model, context: &Context, options: StreamOptions| {
        let index = {
            let mut seen = seen.lock().unwrap();
            seen.push(Request {
                model: model.clone(),
                context: context.clone(),
                reasoning: options.reasoning,
                session_id: options.session_id.clone(),
                transport: options.transport,
                max_tokens: options.max_tokens,
            });
            seen.len() - 1
        };
        let message = respond(index, context);
        let (sender, stream) = event_channel();
        let reason = message.stop_reason;
        let event = match reason {
            StopReason::Error | StopReason::Aborted => AssistantMessageEvent::Error { reason, error: message },
            _ => AssistantMessageEvent::Done { reason, message },
        };
        sender.push(event);
        stream
    });
    (requests, stream_fn)
}

/// A stream function answering with `messages` in order, then plain text.
pub fn sequence(messages: Vec<AssistantMessage>) -> (Arc<Mutex<Vec<Request>>>, StreamFn) {
    scripted(move |index, _| messages.get(index).cloned().unwrap_or_else(|| text("done")))
}

pub fn config(stream_fn: StreamFn) -> AgentLoopConfig {
    AgentLoopConfig { stream_fn: Some(stream_fn), ..AgentLoopConfig::new(model()) }
}

pub fn recorder() -> (Arc<Mutex<Vec<AgentEvent>>>, EventSink) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink_events = events.clone();
    (events, Arc::new(move |event| sink_events.lock().unwrap().push(event)))
}

pub fn kind(event: &AgentEvent) -> String {
    serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()
}

pub fn kinds(events: &[AgentEvent]) -> Vec<String> {
    events.iter().map(kind).collect()
}

pub fn role(message: &Message) -> &'static str {
    match message {
        Message::System(_) => "system",
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
        Message::Custom(_) => "custom",
    }
}

pub fn roles(messages: &[Message]) -> Vec<&'static str> {
    messages.iter().map(role).collect()
}

pub fn user_texts(context: &Context) -> Vec<String> {
    context
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.content.text()),
            _ => None,
        })
        .collect()
}

pub type ExecuteFn = Arc<dyn Fn(&str, Value) -> anyhow::Result<AgentToolResult> + Send + Sync>;

/// A tool with a fixed schema whose `execute` runs a closure and counts calls.
pub struct FnTool {
    pub name: String,
    pub schema: Value,
    pub calls: AtomicUsize,
    pub run: ExecuteFn,
}

impl FnTool {
    pub fn new(
        name: &str,
        schema: Value,
        run: impl Fn(&str, Value) -> anyhow::Result<AgentToolResult> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self { name: name.into(), schema, calls: AtomicUsize::new(0), run: Arc::new(run) })
    }

    /// `echo` with a required string `value`, answering `echoed: <value>`.
    pub fn echo() -> Arc<Self> {
        Self::new(
            "echo",
            json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}),
            |_, args| Ok(AgentToolResult::text(format!("echoed: {}", args["value"].as_str().unwrap_or_default()))),
        )
    }

    pub fn noop() -> Arc<Self> {
        Self::new("noop", json!({"type": "object", "properties": {}}), |_, _| Ok(AgentToolResult::text("done")))
    }

    pub fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AgentTool for FnTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "test tool"
    }

    fn parameters(&self) -> Value {
        self.schema.clone()
    }

    async fn execute(
        &self,
        id: &str,
        args: Value,
        _cancel: CancellationToken,
        _on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.run)(id, args)
    }
}
