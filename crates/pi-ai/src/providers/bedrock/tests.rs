use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::*;
use crate::providers::aws::event_stream::{HeaderValue, Message as Frame, encode};
use crate::providers::http_mock::{Reply, RouteServer};
use crate::providers::mock_server::{cancel_after_first_text, collect, kinds};
use crate::stream::AssistantMessageEvent;
use crate::types::{ContentBlock, ModelCost};

#[path = "request_tests.rs"]
mod request_tests;

pub(super) fn bedrock_model(id: &str, base_url: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "bedrock-converse-stream".into(),
        provider: "amazon-bedrock".into(),
        base_url: base_url.into(),
        reasoning: true,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25, ..Default::default() },
        context_window: 200_000,
        max_tokens: 64_000,
        ..Default::default()
    }
}

fn event(kind: &str, payload: Value) -> Vec<u8> {
    encode(&Frame {
        headers: vec![
            (":message-type".into(), HeaderValue::String("event".into())),
            (":event-type".into(), HeaderValue::String(kind.into())),
            (":content-type".into(), HeaderValue::String("application/json".into())),
        ],
        payload: payload.to_string().into_bytes(),
    })
}

fn exception(kind: &str, message: &str) -> Vec<u8> {
    encode(&Frame {
        headers: vec![
            (":message-type".into(), HeaderValue::String("exception".into())),
            (":exception-type".into(), HeaderValue::String(kind.into())),
        ],
        payload: json!({"message": message}).to_string().into_bytes(),
    })
}

fn stream_reply(frames: Vec<Vec<u8>>) -> Reply {
    let headers = vec![
        ("content-type".to_string(), "application/vnd.amazon.eventstream".to_string()),
        ("x-amzn-requestid".to_string(), "req-123".to_string()),
    ];
    Reply { status: 200, headers, chunks: frames, hang: false }
}

fn context() -> Context {
    Context {
        system_prompt: Some("Be brief.".into()),
        messages: vec![crate::types::Message::user("Hi")],
        tools: Vec::new(),
    }
}

fn bearer_options() -> StreamOptions {
    StreamOptions { api_key: Some("bedrock-key".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_thinking_text_and_usage_with_a_bearer_token() {
    let frames = vec![
        event("messageStart", json!({"role": "assistant", "p": "abc"})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"text": "Plan"}}})),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "sig-"}}}),
        ),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"signature": "1"}}})),
        event("contentBlockStop", json!({"contentBlockIndex": 0})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"text": "Hello"}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"text": " there"}})),
        event("contentBlockStop", json!({"contentBlockIndex": 1})),
        event("messageStop", json!({"stopReason": "end_turn"})),
        event(
            "metadata",
            json!({"usage": {"inputTokens": 100, "outputTokens": 5, "totalTokens": 1_000_105, "cacheWriteInputTokens": 1_000_000,
            "cacheDetails": [{"ttl": "1h", "inputTokens": 150_000}, {"ttl": "5m", "inputTokens": 600_000}, {"ttl": "1h", "inputTokens": 250_000}]}}),
        ),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let items: Arc<Mutex<Vec<Value>>> = Arc::default();
    let responses: Arc<Mutex<Vec<ProviderResponse>>> = Arc::default();
    let (seen, recorded) = (items.clone(), responses.clone());
    let options = StreamOptions {
        reasoning: Some(crate::types::ThinkingLevel::High),
        on_provider_stream_event: Some(Arc::new(move |item, _| seen.lock().unwrap().push(item.clone()))),
        on_response: Some(Arc::new(move |response, _| recorded.lock().unwrap().push(response.clone()))),
        ..bearer_options()
    };
    let (events, message) = collect(BedrockConverseStream.stream(&model, &context(), options)).await;
    assert_eq!(
        kinds(&events),
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_end",
            "text_start",
            "text_delta",
            "text_delta",
            "text_end",
            "done"
        ]
    );
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.raw_stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(message.text(), "Hello there");
    let ContentBlock::Thinking { thinking, thinking_signature, redacted } = &message.content[0] else { panic!() };
    assert_eq!((thinking.as_str(), thinking_signature.as_deref(), *redacted), ("Plan", Some("sig-1"), false));
    assert_eq!((message.usage.input, message.usage.output, message.usage.total_tokens), (100, 5, 1_000_105));
    assert_eq!((message.usage.cache_write, message.usage.cache_write_1h), (1_000_000, Some(400_000)));

    let request = &server.requests()[0];
    assert_eq!(request.path, "/model/us.anthropic.claude-opus-4-8/converse-stream");
    assert_eq!(request.headers["authorization"], "Bearer bedrock-key");
    let body = request.json();
    assert!(body.get("modelId").is_none());
    assert_eq!(body["system"], json!([{"text": "Be brief."}, {"cachePoint": {"type": "default"}}]));
    assert_eq!(body["messages"][0]["content"][1], json!({"cachePoint": {"type": "default"}}));
    assert_eq!(body["additionalModelRequestFields"]["output_config"], json!({"effort": "high"}));
    let items = items.lock().unwrap();
    assert_eq!(items[0], json!({"messageStart": {"role": "assistant"}}));
    assert_eq!(items.len(), 10);
    let responses = responses.lock().unwrap();
    assert_eq!((responses[0].status, responses[0].headers["x-amzn-requestid"].as_str()), (200, "req-123"));
}

#[tokio::test]
async fn streams_tool_calls_and_keeps_empty_property_names() {
    let input = r#"{"path":"/workspace/file.js","edits":[{"oldText":"a","newText":"b","":""}]}"#;
    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event(
            "contentBlockStart",
            json!({"contentBlockIndex": 0, "start": {"toolUse": {"toolUseId": "tool-1", "name": "edit"}}}),
        ),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": &input[..20]}}})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"toolUse": {"input": &input[20..]}}})),
        event("contentBlockStop", json!({"contentBlockIndex": 0})),
        event("messageStop", json!({"stopReason": "tool_use"})),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let (events, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert!(kinds(&events).contains(&"toolcall_end".to_string()));
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), call.name.as_str()), ("tool-1", "edit"));
    assert_eq!(Value::Object(call.arguments.clone()), serde_json::from_str::<Value>(input).unwrap());
}

#[tokio::test]
async fn signs_requests_with_environment_credentials() {
    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event("messageStop", json!({"stopReason": "end_turn"})),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("anthropic.claude-3-5-sonnet-20240620-v1:0", &server.base_url);
    let env = BTreeMap::from([
        ("AWS_ACCESS_KEY_ID".to_string(), "AKIDEXAMPLE".to_string()),
        ("AWS_SECRET_ACCESS_KEY".to_string(), "secret".to_string()),
        ("AWS_SESSION_TOKEN".to_string(), "session".to_string()),
        ("AWS_REGION".to_string(), "us-west-2".to_string()),
    ]);
    let options = StreamOptions {
        env,
        headers: BTreeMap::from([("x-custom".into(), "v".into()), ("X-Amz-Date".into(), "bad".into())]),
        ..StreamOptions::default()
    };
    let (_, message) = collect(BedrockConverseStream.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = &server.requests()[0];
    assert_eq!(request.path, "/model/anthropic.claude-3-5-sonnet-20240620-v1%3A0/converse-stream");
    let authorization = &request.headers["authorization"];
    assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"), "{authorization}");
    assert!(authorization.contains("/us-west-2/bedrock/aws4_request"), "{authorization}");
    assert!(authorization.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-custom"));
    assert_eq!(request.headers["x-amz-security-token"], "session");
    assert_ne!(request.headers["x-amz-date"], "bad");
}

#[tokio::test]
async fn reports_http_errors_with_prefixes_and_diagnostics() {
    let server = RouteServer::start(|_| {
        Reply::json(400, &json!({"message": "data retention mode 'default' is not available"}))
            .header("x-amzn-errortype", "ValidationException:http://internal.amazon.com/coral/")
            .header("x-amzn-requestid", "req-400")
    })
    .await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let (events, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert_eq!(kinds(&events), ["error"]);
    let error = message.error_message.unwrap();
    assert!(
        error.starts_with("Validation error: data retention mode 'default' is not available See https://docs.aws"),
        "{error}"
    );
    assert_eq!(message.diagnostics[0]["type"], "bedrock_response_failure");
    assert_eq!(
        message.diagnostics[0]["details"],
        json!({"status": 400, "errorCode": "ValidationException", "requestId": "req-400"})
    );
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn retries_throttling_before_the_stream_starts() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let server = RouteServer::start(move |_| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            return Reply::json(429, &json!({"message": "Too many requests"}))
                .header("x-amzn-errortype", "ThrottlingException");
        }
        stream_reply(vec![
            event("messageStart", json!({"role": "assistant"})),
            event("messageStop", json!({"stopReason": "end_turn"})),
        ])
    })
    .await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let (_, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn mid_stream_exceptions_and_error_stops_fail_the_turn() {
    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "partial"}})),
        exception("throttlingException", "Too many tokens, please wait before trying again."),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let (_, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Throttling error: Too many tokens, please wait before trying again.")
    );
    assert_eq!(message.text(), "partial");
    assert_eq!(message.diagnostics[0]["details"], json!({"requestId": "req-123"}));

    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event("messageStop", json!({"stopReason": "guardrail_intervened"})),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let (_, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Provider stopped with: guardrail_intervened"));
    assert_eq!(message.raw_stop_reason.as_deref(), Some("guardrail_intervened"));
}

#[tokio::test]
async fn keeps_redacted_reasoning_as_a_base64_signature() {
    let payload = vec![0u8, 1, 2, 250, 251, 252, 253];
    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"redactedContent": STANDARD.encode(&payload[..3])}}}),
        ),
        event(
            "contentBlockDelta",
            json!({"contentBlockIndex": 0, "delta": {"reasoningContent": {"redactedContent": STANDARD.encode(&payload[3..])}}}),
        ),
        event("contentBlockDelta", json!({"contentBlockIndex": 1, "delta": {"text": "done"}})),
        event("messageStop", json!({"stopReason": "end_turn"})),
    ];
    let server = RouteServer::start(move |_| stream_reply(frames.clone())).await;
    let model = bedrock_model("openai.gpt-5.6-sol", &server.base_url);
    let (_, message) = collect(BedrockConverseStream.stream(&model, &context(), bearer_options())).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let ContentBlock::Thinking { thinking, thinking_signature, redacted } = &message.content[0] else { panic!() };
    assert_eq!(thinking, "[Reasoning redacted]");
    assert!(*redacted);
    assert_eq!(thinking_signature.as_deref(), Some(STANDARD.encode(&payload).as_str()));
    assert_eq!(message.content[1], ContentBlock::text("done"));
}

#[tokio::test]
async fn aborts_an_open_stream() {
    let frames = vec![
        event("messageStart", json!({"role": "assistant"})),
        event("contentBlockDelta", json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}})),
    ];
    let server = RouteServer::start(move |_| Reply { hang: true, ..stream_reply(frames.clone()) }).await;
    let model = bedrock_model("us.anthropic.claude-opus-4-8", &server.base_url);
    let options = bearer_options();
    let cancel = options.cancel.clone();
    let message = cancel_after_first_text(BedrockConverseStream.stream(&model, &context(), options), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_message.as_deref(), Some("Request was aborted"));
    assert!(message.diagnostics.is_empty());
}

#[tokio::test]
async fn unsupported_images_fail_before_sending() {
    let model = bedrock_model("us.anthropic.claude-opus-4-8", "http://127.0.0.1:9");
    let image = ContentBlock::Image { data: "AA==".into(), mime_type: "image/bmp".into() };
    let user = crate::types::Message::User(crate::types::UserMessage {
        content: crate::types::UserContent::Blocks(vec![image]),
        timestamp: 0,
    });
    let context = Context { messages: vec![user], ..Context::default() };
    let events: Vec<AssistantMessageEvent> =
        futures::StreamExt::collect(BedrockConverseStream.stream(&model, &context, bearer_options())).await;
    let message = events.last().and_then(AssistantMessageEvent::final_message).unwrap();
    assert_eq!(message.error_message.as_deref(), Some("Unknown image type: image/bmp"));
}
