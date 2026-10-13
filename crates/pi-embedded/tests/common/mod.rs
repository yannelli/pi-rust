//! Helpers shared by the runtime tests (upstream `test/runtime.test.ts` fixtures).
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use pi_agent::StreamFn;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Context, Message, Model, StopReason, ToolCall, Usage,
    event_channel, now_ms,
};
use pi_embedded::{
    CreateSessionOptions, EmbeddedHost, EmbeddedRuntime, EmbeddedSession, FnTool, InvocationPhase, MemorySessionStore,
    SessionEvent, SessionStore, ToolDeclaration, ToolEffect, ToolInvocation, ToolResult,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub fn model() -> Model {
    serde_json::from_value(json!({
        "id": "test", "name": "Test", "api": "openai-completions", "provider": "openai",
        "baseUrl": "https://example.test", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 8192, "maxTokens": 1024
    }))
    .unwrap()
}

pub fn assistant(content: Vec<ContentBlock>) -> AssistantMessage {
    let has_call = content.iter().any(|part| matches!(part, ContentBlock::ToolCall(_)));
    AssistantMessage {
        content,
        usage: Usage { input: 10, output: 2, total_tokens: 12, ..Usage::default() },
        stop_reason: if has_call { StopReason::ToolUse } else { StopReason::Stop },
        timestamp: now_ms(),
        ..AssistantMessage::empty(&model())
    }
}

pub fn answer() -> AssistantMessage {
    assistant(vec![ContentBlock::text("answer")])
}

pub fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ContentBlock {
    let arguments = arguments.as_object().cloned().unwrap_or_default();
    ContentBlock::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments, ..ToolCall::default() })
}

pub fn call(id: &str, name: &str) -> AssistantMessage {
    assistant(vec![tool_call(id, name, json!({}))])
}

pub fn result(text: &str) -> ToolResult {
    ToolResult::text(text, json!({"text": text}))
}

pub fn declaration(name: &str, effect: ToolEffect) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        description: "A host tool".into(),
        parameters: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        capabilities: Vec::new(),
        effect,
        approval: false,
    }
}

/// A tool that counts executions and returns `result("completed")`.
pub fn counted(declaration: ToolDeclaration, count: Arc<Mutex<usize>>) -> Arc<FnTool> {
    Arc::new(FnTool::new(declaration, move |_, _| {
        let count = count.clone();
        async move {
            *count.lock().unwrap() += 1;
            Ok(result("completed"))
        }
    }))
}

pub type Requests = Arc<Mutex<Vec<(Model, Context)>>>;

pub struct Script {
    pub stream: StreamFn,
    pub requests: Requests,
    pub started: CancellationToken,
}

/// Replays `responses` in order; the first waits on `gate` (or cancellation) after starting.
pub fn script(responses: Vec<AssistantMessage>, gate: Option<CancellationToken>) -> Script {
    let requests: Requests = Arc::default();
    let started = CancellationToken::new();
    let (recorded, signal) = (requests.clone(), started.clone());
    let stream: StreamFn = Arc::new(move |selected, context, options| {
        let (sender, events) = event_channel();
        let index = {
            let mut requests = recorded.lock().unwrap();
            requests.push((selected.clone(), context.clone()));
            requests.len() - 1
        };
        let mut message = responses.get(index).cloned().unwrap_or_else(answer);
        let (gate, signal) = (gate.clone(), signal.clone());
        tokio::spawn(async move {
            sender.push(AssistantMessageEvent::Start { partial: message.clone() });
            if index == 0 {
                signal.cancel();
                if let Some(gate) = gate {
                    tokio::select! { _ = gate.cancelled() => {}, _ = options.cancel.cancelled() => {} }
                }
            }
            if options.cancel.is_cancelled() {
                message.stop_reason = StopReason::Aborted;
                message.error_message = Some("cancelled".into());
                sender.push(AssistantMessageEvent::Error { reason: StopReason::Aborted, error: message });
                return;
            }
            for (content_index, part) in message.content.iter().enumerate() {
                if let Some(text) = part.as_text() {
                    let (delta, partial) = (text.to_string(), message.clone());
                    sender.push(AssistantMessageEvent::TextDelta { content_index, delta, partial });
                }
            }
            sender.push(AssistantMessageEvent::Done { reason: message.stop_reason, message });
        });
        events
    });
    Script { stream, requests, started }
}

pub fn host(store: Arc<dyn SessionStore>, stream: StreamFn) -> EmbeddedHost {
    EmbeddedHost::new(store, stream)
}

pub async fn open(host: EmbeddedHost, id: &str) -> EmbeddedSession {
    let runtime = EmbeddedRuntime::new(host).unwrap();
    runtime.create(CreateSessionOptions { id: id.into(), model: model(), system_prompt: None }).await.unwrap()
}

pub fn restore_runtime(host: EmbeddedHost) -> EmbeddedRuntime {
    EmbeddedRuntime::new(host).unwrap()
}

/// A saved running checkpoint holding `messages`, with every tool call recorded in `phase`.
pub async fn saved(messages: Vec<Message>, phase: InvocationPhase) -> Arc<MemorySessionStore> {
    let store = Arc::new(MemorySessionStore::new());
    let session = open(host(store.clone(), script(Vec::new(), None).stream), "session").await;
    let mut snapshot = session.snapshot();
    snapshot.revision += 1;
    snapshot.status = pi_embedded::SessionStatus::Running;
    snapshot.needs_response = true;
    snapshot.invocations = messages
        .iter()
        .filter_map(|message| match message {
            Message::Assistant(assistant) => Some(assistant.tool_calls().cloned().collect::<Vec<_>>()),
            _ => None,
        })
        .flatten()
        .map(|call| ToolInvocation {
            session_id: "session".into(),
            id: call.id,
            name: call.name,
            args: call.arguments,
            effect: ToolEffect::Mutation,
            phase,
            result: (phase == InvocationPhase::Completed).then(|| result("saved outcome")),
            approved: None,
        })
        .collect();
    snapshot.messages = messages;
    store.save(&snapshot, Some(snapshot.revision - 1)).await.unwrap();
    store
}

/// Rewrite the saved checkpoint with `edit`.
pub async fn edit_saved(store: &MemorySessionStore, edit: impl FnOnce(&mut pi_embedded::SessionSnapshot)) {
    let mut snapshot = store.load("session").await.unwrap().unwrap();
    edit(&mut snapshot);
    snapshot.revision += 1;
    store.save(&snapshot, Some(snapshot.revision - 1)).await.unwrap();
}

/// Records events; approves every approval request with `approved` when set.
pub fn record(session: &EmbeddedSession, approve: Option<bool>) -> Arc<Mutex<Vec<SessionEvent>>> {
    let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
    let (sink, handle) = (events.clone(), session.clone());
    session.subscribe(move |event| {
        sink.lock().unwrap().push(event.clone());
        if let (Some(approved), pi_embedded::EventPayload::Approval { invocation }) = (approve, &event.payload) {
            let (session, id) = (handle.clone(), invocation.id.clone());
            tokio::spawn(async move { session.approve(&id, approved).await.unwrap() });
        }
    });
    events
}

pub fn user_texts(session: &EmbeddedSession) -> Vec<String> {
    session
        .snapshot()
        .messages
        .iter()
        .filter_map(|message| match message {
            Message::User(user) => Some(user.content.text()),
            _ => None,
        })
        .collect()
}

pub fn tool_results(messages: &[Message]) -> Vec<&pi_ai::ToolResultMessage> {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}
