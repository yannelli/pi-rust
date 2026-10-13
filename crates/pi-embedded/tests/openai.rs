//! Port of upstream `test/openai.test.ts`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use pi_ai::{
    AssistantMessageEvent, ContentBlock, Context, Message, Model, StopReason, StreamOptions, UserContent, UserMessage,
};
use pi_embedded::{
    CredentialStore, NetworkAdapter, NetworkRequest, NetworkResponse, create_model_compactor, create_openai_stream,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "hosted", "name": "Hosted", "api": "openai-completions", "provider": "openai",
        "baseUrl": "https://provider.test/v1/", "reasoning": false, "input": ["text"],
        "cost": {"input": 2, "output": 8, "cacheRead": 1, "cacheWrite": 2}, "contextWindow": 8192, "maxTokens": 1024
    }))
    .unwrap()
}

fn context() -> Context {
    let user = Message::User(UserMessage { content: UserContent::Text("hello".into()), timestamp: 1 });
    Context { system_prompt: None, messages: vec![user], tools: Vec::new() }
}

struct Key;

#[async_trait]
impl CredentialStore for Key {
    async fn get(&self, _: &str) -> anyhow::Result<Option<String>> {
        Ok(Some("private-key".into()))
    }

    async fn set(&self, _: &str, _: Option<String>) -> anyhow::Result<()> {
        Ok(())
    }
}

fn frame(value: Value) -> String {
    match value {
        Value::String(text) => format!("data: {text}\r\n\r\n"),
        value => format!("data: {value}\r\n\r\n"),
    }
}

/// Sets the flag when the response body is dropped.
struct Closed(Arc<AtomicBool>);

impl Drop for Closed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct Wire {
    body: String,
    status: u16,
    requests: Arc<Mutex<Vec<NetworkRequest>>>,
    closed: Arc<AtomicBool>,
}

#[async_trait]
impl NetworkAdapter for Wire {
    async fn request(&self, request: NetworkRequest, _: CancellationToken) -> anyhow::Result<NetworkResponse> {
        self.requests.lock().unwrap().push(request);
        let pieces: Vec<anyhow::Result<String>> =
            self.body.as_bytes().chunks(3).map(|piece| Ok(String::from_utf8(piece.to_vec()).unwrap())).collect();
        let guard = Closed(self.closed.clone());
        let body: BoxStream<'static, anyhow::Result<String>> = futures::stream::iter(pieces)
            .map(move |piece| {
                let _ = &guard;
                piece
            })
            .boxed();
        Ok(NetworkResponse { status: self.status, body })
    }
}

fn wire(body: String, status: u16) -> Wire {
    Wire { body, status, requests: Arc::default(), closed: Arc::default() }
}

async fn run(wire: &Wire, cancel: CancellationToken) -> (Vec<AssistantMessageEvent>, pi_ai::AssistantMessage) {
    let stream = create_openai_stream(Arc::new(wire.clone()), Arc::new(Key));
    let options = StreamOptions { cancel, ..StreamOptions::default() };
    let events: Vec<AssistantMessageEvent> = stream(&model(), &context(), options).collect().await;
    let last = events.last().and_then(|event| event.final_message()).cloned().expect("terminal event");
    (events, last)
}

fn kinds(events: &[AssistantMessageEvent]) -> Vec<String> {
    events.iter().map(|event| serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

#[tokio::test]
async fn decodes_fragmented_sse_emits_deltas_reports_usage_and_closes_the_response() {
    let body = ": keepalive\r\n\r\n".to_string()
        + &frame(
            json!({"id": "response", "model": "hosted-version", "choices": [{"index": 0, "delta": {"content": "hello "}}]}),
        )
        + &frame(json!({"choices": [{"index": 0, "delta": {"content": "world"}, "finish_reason": "stop"}]}))
        + &frame(json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25,
            "prompt_tokens_details": {"cached_tokens": 4}, "completion_tokens_details": {"reasoning_tokens": 2}}}))
        + &frame(json!("[DONE]"));
    let wire = wire(body, 200);
    let (events, message) = run(&wire, CancellationToken::new()).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]);
    assert_eq!(message.response_id.as_deref(), Some("response"));
    assert_eq!(message.response_model.as_deref(), Some("hosted-version"));
    assert_eq!(message.content, vec![ContentBlock::text("hello world")]);
    assert_eq!(message.stop_reason, StopReason::Stop);
    let usage = message.usage;
    assert_eq!(
        (usage.input, usage.output, usage.cache_read, usage.total_tokens, usage.reasoning),
        (16, 5, 4, 25, Some(2))
    );
    assert!((usage.cost.total - 0.000076).abs() < 1e-12);
    let request = wire.requests.lock().unwrap()[0].clone();
    assert_eq!(request.url, "https://provider.test/v1/chat/completions");
    assert_eq!(request.headers["Authorization"], "Bearer private-key");
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!((body["model"].clone(), body["stream"].clone()), (json!("hosted"), json!(true)));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert!(wire.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn assembles_interleaved_tool_argument_and_name_fragments() {
    let body = frame(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
            {"index": 0, "id": "a", "function": {"name": "read_", "arguments": "{\"path\":"}},
            {"index": 1, "id": "b", "function": {"name": "search", "arguments": "{\"query\":"}}]}}]}))
        + &frame(json!({"choices": [{"index": 0, "finish_reason": "tool_calls", "delta": {"tool_calls": [
            {"index": 1, "function": {"arguments": "\"needle\"}"}},
            {"index": 0, "function": {"name": "file", "arguments": "\"a.txt\"}"}}]}}]}))
        + &frame(json!("[DONE]"));
    let (events, message) = run(&wire(body, 200), CancellationToken::new()).await;
    let content = serde_json::to_value(&message.content).unwrap();
    assert_eq!(
        content,
        json!([
            {"type": "toolCall", "id": "a", "name": "read_file", "arguments": {"path": "a.txt"}},
            {"type": "toolCall", "id": "b", "name": "search", "arguments": {"query": "needle"}}
        ])
    );
    assert_eq!(kinds(&events).iter().filter(|kind| *kind == "toolcall_end").count(), 2);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn reports_malformed_streams_as_error_streams() {
    let call = |arguments: &str| {
        frame(json!({"choices": [{"index": 0, "finish_reason": "tool_calls",
            "delta": {"tool_calls": [{"index": 0, "id": "call", "function": {"name": "read", "arguments": arguments}}]}}]}))
            + &frame(json!("[DONE]"))
    };
    let cases = [
        (
            "missing DONE",
            frame(json!({"choices": [{"index": 0, "delta": {"content": "partial"}, "finish_reason": "stop"}]})),
        ),
        (
            "missing finish reason",
            frame(json!({"choices": [{"index": 0, "delta": {"content": "partial"}}]})) + &frame(json!("[DONE]")),
        ),
        ("malformed JSON", "data: broken\n\n".to_string()),
        ("provider streaming error", frame(json!({"error": {"message": "private upstream diagnostic"}}))),
        ("malformed arguments", call("{")),
        ("array arguments", call("[]")),
    ];
    for (name, body) in cases {
        let (events, message) = run(&wire(body, 200), CancellationToken::new()).await;
        assert_eq!(kinds(&events).last().map(String::as_str), Some("error"), "{name}");
        assert_eq!(message.stop_reason, StopReason::Error, "{name}");
        assert!(!message.error_message.unwrap_or_default().contains("private upstream diagnostic"), "{name}");
    }
}

#[tokio::test]
async fn preserves_a_length_stop_without_executing_incomplete_tool_arguments() {
    let body = frame(json!({"choices": [{"index": 0, "finish_reason": "length",
            "delta": {"tool_calls": [{"index": 0, "id": "call", "function": {"name": "read", "arguments": "{\"path\":"}}]}}]}))
        + &frame(json!("[DONE]"));
    let (_, message) = run(&wire(body, 200), CancellationToken::new()).await;
    assert_eq!(message.stop_reason, StopReason::Length);
    assert_eq!(serde_json::to_value(&message.content).unwrap()[0]["arguments"], json!({}));
}

#[tokio::test]
async fn closes_failed_http_responses_and_omits_provider_bodies_and_credentials_from_errors() {
    let wire = wire("private-key private diagnostic".into(), 401);
    let (_, message) = run(&wire, CancellationToken::new()).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Provider HTTP 401"));
    assert!(wire.closed.load(Ordering::SeqCst));
}

struct Offline(Arc<AtomicBool>);

#[async_trait]
impl NetworkAdapter for Offline {
    async fn request(&self, _: NetworkRequest, _: CancellationToken) -> anyhow::Result<NetworkResponse> {
        self.0.store(true, Ordering::SeqCst);
        anyhow::bail!("offline")
    }
}

#[tokio::test]
async fn reports_transport_failures_and_cancellation_through_error_events() {
    let requested = Arc::new(AtomicBool::new(false));
    let stream = create_openai_stream(Arc::new(Offline(requested.clone())), Arc::new(Key));
    let message = stream(&model(), &context(), StreamOptions::default()).result(&model()).await;
    assert_eq!((message.stop_reason, message.error_message.as_deref()), (StopReason::Error, Some("offline")));
    requested.store(false, Ordering::SeqCst);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let options = StreamOptions { cancel, ..StreamOptions::default() };
    let message = stream(&model(), &context(), options).result(&model()).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(!requested.load(Ordering::SeqCst));
}

struct Hanging(Arc<AtomicBool>, Arc<Mutex<Option<CancellationToken>>>);

#[async_trait]
impl NetworkAdapter for Hanging {
    async fn request(&self, _: NetworkRequest, cancel: CancellationToken) -> anyhow::Result<NetworkResponse> {
        *self.1.lock().unwrap() = Some(cancel);
        let guard = Closed(self.0.clone());
        let first = futures::stream::once(async {
            Ok(frame(json!({"choices": [{"index": 0, "delta": {"content": "partial"}}]})))
        });
        let body = first.chain(futures::stream::pending()).map(move |item| {
            let _ = &guard;
            item
        });
        Ok(NetworkResponse { status: 200, body: body.boxed() })
    }
}

#[tokio::test]
async fn aborts_an_active_streaming_request_and_closes_its_body() {
    let (closed, requested) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(None)));
    let stream = create_openai_stream(Arc::new(Hanging(closed.clone(), requested.clone())), Arc::new(Key));
    let cancel = CancellationToken::new();
    let mut events = stream(&model(), &context(), StreamOptions { cancel: cancel.clone(), ..StreamOptions::default() });
    let mut last = None;
    while let Some(event) = events.next().await {
        if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
            cancel.cancel();
        }
        last = event.final_message().cloned();
    }
    let message = last.unwrap();
    assert_eq!((message.stop_reason, message.text()), (StopReason::Aborted, "partial".to_string()));
    assert!(requested.lock().unwrap().as_ref().unwrap().is_cancelled());
    assert!(closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn uses_the_same_provider_adapter_for_compaction_and_reports_its_usage() {
    let body = frame(json!({"choices": [{"index": 0, "delta": {"content": "summary"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}))
        + &frame(json!("[DONE]"));
    let wire = wire(body, 200);
    let compactor = create_model_compactor(create_openai_stream(Arc::new(wire.clone()), Arc::new(Key)));
    let messages = context().messages;
    let compacted = compactor.compact(messages.clone(), model(), CancellationToken::new()).await.unwrap();
    assert_eq!(compacted.summary, "summary");
    let usage = compacted.usage.unwrap();
    assert_eq!((usage.input, usage.output, usage.total_tokens), (10, 2, 12));
    let body: Value = serde_json::from_str(&wire.requests.lock().unwrap()[0].body).unwrap();
    assert_eq!(body["messages"][1]["content"], serde_json::to_string(&messages).unwrap());
}
