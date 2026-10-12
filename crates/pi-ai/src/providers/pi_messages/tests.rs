use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use serde_json::{Value, json};

use super::PiMessages;
use crate::providers::mock_http::{Body, MockHttp, Reply};
use crate::providers::mock_server::{cancel_after_first_text, collect, kinds};
use crate::registry::{ApiProvider, ProviderResponse, StreamOptions, api_provider};
use crate::stream::AssistantMessageEvent;
use crate::types::{ContentBlock, Context, Message, Model, ModelCost, StopReason, Usage, UserContent, UserMessage};

fn model(base_url: &str) -> Model {
    Model {
        id: "auto".into(),
        name: "Radius Auto".into(),
        api: "pi-messages".into(),
        provider: "radius".into(),
        base_url: format!("{base_url}/v1"),
        input: vec!["text".into()],
        cost: ModelCost { input: 1.0, output: 2.0, cache_read: 0.1, cache_write: 0.2, ..ModelCost::default() },
        context_window: 128_000,
        max_tokens: 16_384,
        ..Model::default()
    }
}

fn context() -> Context {
    let user = Message::User(UserMessage { content: UserContent::Text("Hello".into()), timestamp: 7 });
    Context { messages: vec![user], ..Context::default() }
}

fn usage() -> Value {
    json!({"input": 10, "output": 5, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 15,
        "cost": {"input": 0.1, "output": 0.2, "cacheRead": 0, "cacheWrite": 0, "total": 0.3}})
}

fn wire(events: &[Value]) -> Reply {
    Reply::sse(events.iter().map(|event| format!("data: {event}\n\n")).collect())
}

fn keyed() -> StreamOptions {
    StreamOptions { api_key: Some("test-key".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_text_and_tool_calls_and_resolves_the_terminal_message() {
    let tool_call = json!({"type": "toolCall", "id": "call_1", "name": "read", "arguments": {"path": "a.txt"}});
    let server = MockHttp::start(vec![wire(&[
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "Hel"}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "lo"}),
        json!({"type": "text_end", "contentIndex": 0, "content": "Hello"}),
        json!({"type": "toolcall_start", "contentIndex": 1, "id": "call_1", "toolName": "read"}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "{\"path\":"}),
        json!({"type": "toolcall_delta", "contentIndex": 1, "delta": "\"a.txt\"}"}),
        json!({"type": "toolcall_end", "contentIndex": 1, "toolCall": tool_call}),
        json!({"type": "done", "reason": "toolUse", "usage": usage(), "responseId": "resp_1", "providerThinkingLevel": "high"}),
    ])])
    .await;
    let options = StreamOptions {
        session_id: Some("session-1".into()),
        tool_choice: Some("auto".into()),
        max_tokens: Some(100),
        headers: BTreeMap::from([("x-custom".to_string(), "1".to_string())]),
        ..keyed()
    };
    let mut stream = PiMessages.stream(&model(&server.base_url), &context(), options);
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    let AssistantMessageEvent::Start { partial } = &events[0] else { panic!("start first") };
    assert_eq!(partial.stop_reason, StopReason::Pending);
    assert_eq!(kinds(&events).iter().filter(|kind| *kind == "toolcall_end").count(), 1);
    assert!(kinds(&events).contains(&"text_delta".to_string()));
    let message = events.last().unwrap().final_message().unwrap().clone();
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage, serde_json::from_value::<Usage>(usage()).unwrap());
    assert_eq!(
        (message.response_id.as_deref(), message.provider_thinking_level.as_deref()),
        (Some("resp_1"), Some("high"))
    );
    assert_eq!((message.model.as_str(), message.provider.as_str()), ("auto", "radius"));
    assert_eq!(message.content[0], ContentBlock::text("Hello"));
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), call.name.as_str(), &call.arguments["path"]), ("call_1", "read", &json!("a.txt")));
    let request = server.request(0);
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(
        (request.headers["authorization"].as_str(), request.headers["x-custom"].as_str()),
        ("Bearer test-key", "1")
    );
    assert_eq!(
        request.json(),
        json!({"model": "auto", "context": {"messages": [{"role": "user", "content": "Hello", "timestamp": 7}]},
            "options": {"maxTokens": 100, "sessionId": "session-1", "toolChoice": "auto"}})
    );
}

#[tokio::test]
async fn forwards_wire_events_and_reports_response_headers() {
    let events = [
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "Hello", "gatewayField": "upstream-value"}),
        json!({"type": "text_end", "contentIndex": 0, "content": "Hello"}),
        json!({"type": "done", "reason": "stop", "usage": usage(), "responseId": "resp_1"}),
    ];
    let server = MockHttp::start(vec![wire(&events).header("x-pi-gateway-upstream-provider", "anthropic")]).await;
    let (seen, responses) =
        (Arc::new(Mutex::new(Vec::<Value>::new())), Arc::new(Mutex::new(Vec::<ProviderResponse>::new())));
    let (seen_hook, response_hook) = (seen.clone(), responses.clone());
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event: &Value, _: &Model| {
            seen_hook.lock().unwrap().push(event.clone())
        })),
        on_response: Some(Arc::new(move |response: &ProviderResponse, _: &Model| {
            response_hook.lock().unwrap().push(response.clone())
        })),
        ..keyed()
    };
    let (_, message) = collect(PiMessages.stream(&model(&server.base_url), &context(), options)).await;
    assert_eq!((message.stop_reason, message.text().as_str()), (StopReason::Stop, "Hello"));
    assert_eq!(*seen.lock().unwrap(), events);
    assert_eq!(responses.lock().unwrap()[0].headers["x-pi-gateway-upstream-provider"], "anthropic");
}

#[tokio::test]
async fn surfaces_backend_error_responses_with_diagnostics() {
    let body = json!({"error": {"message": "Token expired", "code": "unauthorized"}});
    let server = MockHttp::start(vec![Reply::json(401, &body)]).await;
    let (_, message) = collect(PiMessages.stream(&model(&server.base_url), &context(), keyed())).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("401 Unauthorized: Token expired (unauthorized)"));
    let diagnostic = &message.diagnostics[0];
    assert_eq!(diagnostic["type"], "pi_messages_response_failure");
    assert_eq!(
        (diagnostic["details"]["status"].clone(), diagnostic["details"]["error"].clone()),
        (json!(401), body["error"].clone())
    );
    assert_eq!(diagnostic["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn propagates_error_events_and_missing_terminal_events() {
    let server = MockHttp::start(vec![wire(&[
        json!({"type": "start"}),
        json!({"type": "error", "reason": "error", "usage": usage(), "errorMessage": "Upstream failed"}),
    ])])
    .await;
    let (_, message) = collect(PiMessages.stream(&model(&server.base_url), &context(), keyed())).await;
    assert_eq!((message.stop_reason, message.error_message.as_deref()), (StopReason::Error, Some("Upstream failed")));
    assert_eq!(message.usage.total_tokens, 15);

    let server = MockHttp::start(vec![wire(&[
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"}),
    ])])
    .await;
    let (_, message) = collect(PiMessages.stream(&model(&server.base_url), &context(), keyed())).await;
    assert_eq!(message.error_message.as_deref(), Some("radius stream ended without a terminal event"));

    let (events, message) =
        collect(PiMessages.stream(&model("http://127.0.0.1:1"), &context(), StreamOptions::default())).await;
    assert_eq!(kinds(&events), ["error"]);
    assert!(message.error_message.unwrap().contains("No API key provided"));
}

#[tokio::test]
async fn aborts_and_is_registered() {
    let frames = vec![
        format!("data: {}\n\n", json!({"type": "start"})),
        format!("data: {}\n\n", json!({"type": "text_start", "contentIndex": 0})),
        format!("data: {}\n\n", json!({"type": "text_delta", "contentIndex": 0, "delta": "partial"})),
    ];
    let server = MockHttp::start(vec![Reply { body: Body::ChunksThenHang(frames), ..Reply::sse(Vec::new()) }]).await;
    let options = keyed();
    let cancel = options.cancel.clone();
    let message =
        cancel_after_first_text(PiMessages.stream(&model(&server.base_url), &context(), options), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(api_provider("pi-messages").is_some());
}

#[tokio::test]
async fn streams_thinking_and_serializes_image_context() {
    let server = MockHttp::start(vec![wire(&[
        json!({"type": "start"}),
        json!({"type": "thinking_start", "contentIndex": 0}),
        json!({"type": "thinking_delta", "contentIndex": 0, "delta": "Plan"}),
        json!({"type": "thinking_end", "contentIndex": 0, "content": "Plan", "contentSignature": "sig", "redacted": false}),
        json!({"type": "done", "reason": "stop", "usage": usage()}),
    ])])
    .await;
    let image = ContentBlock::Image { data: "aGVsbG8=".into(), mime_type: "image/png".into() };
    let user = Message::User(UserMessage {
        content: UserContent::Blocks(vec![ContentBlock::text("see"), image]),
        timestamp: 3,
    });
    let context = Context { system_prompt: Some("Be brief".into()), messages: vec![user], ..Context::default() };
    let (events, message) = collect(PiMessages.stream(&model(&server.base_url), &context, keyed())).await;
    assert_eq!(kinds(&events), ["start", "thinking_start", "thinking_delta", "thinking_end", "done"]);
    let ContentBlock::Thinking { thinking, thinking_signature, .. } = &message.content[0] else { panic!("thinking") };
    assert_eq!((thinking.as_str(), thinking_signature.as_deref()), ("Plan", Some("sig")));
    assert!((message.usage.cost.total - 0.3).abs() < 1e-12);
    let messages = server.request(0).json()["context"]["messages"].clone();
    assert_eq!(messages[0], json!({"role": "system", "content": "Be brief", "timestamp": 0}));
    assert_eq!(messages[1]["content"][1], json!({"type": "image", "data": "aGVsbG8=", "mimeType": "image/png"}));
}
