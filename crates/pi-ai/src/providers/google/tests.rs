use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::GoogleGenerativeAI;
use crate::providers::mock_http::{MockHttp, Reply};
use crate::providers::mock_server::{cancel_after_first_text, collect, frame, kinds, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{
    ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool, UserContent, UserMessage, now_ms,
};

const API: &str = "google-generative-ai";

fn chunk(value: Value) -> String {
    frame(None, &value)
}

fn parts(parts: Value) -> String {
    chunk(json!({"candidates": [{"content": {"role": "model", "parts": parts}}]}))
}

fn finish(reason: &str) -> String {
    chunk(json!({
        "candidates": [{"finishReason": reason}],
        "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 20, "candidatesTokenCount": 30,
            "thoughtsTokenCount": 10, "totalTokenCount": 140}
    }))
}

fn model(base_url: &str, id: &str) -> Model {
    Model { id: id.into(), ..test_model(API, "google", base_url) }
}

fn context() -> Context {
    Context { system_prompt: Some("Be brief".into()), messages: vec![Message::user("hi")], ..Context::default() }
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("gem-key".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_thinking_and_text_with_signatures_usage_and_cost() {
    let server = MockHttp::start(vec![Reply::sse(vec![
        chunk(json!({"responseId": "resp-1", "candidates": [{"content": {"parts": [{"text": "Plan", "thought": true, "thoughtSignature": "c2lnMQ=="}]}}]})),
        parts(json!([{"text": " more", "thought": true}])),
        parts(json!([{"text": "Hel"}])),
        parts(json!([{"text": "lo", "thoughtSignature": "c2lnMg=="}])),
        finish("STOP"),
    ])])
    .await;
    let model = model(&server.base_url, "gemini-2.5-flash");
    let options = StreamOptions { reasoning: Some(ThinkingLevel::Medium), temperature: Some(0.5), ..options() };
    let (events, message) = collect(GoogleGenerativeAI.stream(&model, &context(), options)).await;
    assert_eq!(
        kinds(&events),
        [
            "start",
            "thinking_start",
            "thinking_delta",
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
    assert_eq!(message.response_id.as_deref(), Some("resp-1"));
    assert_eq!(message.raw_stop_reason.as_deref(), Some("STOP"));
    let ContentBlock::Thinking { thinking, thinking_signature, .. } = &message.content[0] else { panic!("thinking") };
    assert_eq!((thinking.as_str(), thinking_signature.as_deref()), ("Plan more", Some("c2lnMQ==")));
    let ContentBlock::Text { text, text_signature } = &message.content[1] else { panic!("text") };
    assert_eq!((text.as_str(), text_signature.as_deref()), ("Hello", Some("c2lnMg==")));
    let usage = message.usage;
    assert_eq!(
        (usage.input, usage.output, usage.cache_read, usage.reasoning, usage.total_tokens),
        (80, 40, 20, Some(10), 140)
    );
    assert!((usage.cost.total - (80.0 + 40.0 * 2.0 + 20.0 * 0.5) / 1e6).abs() < 1e-12);

    let request = server.request(0);
    assert_eq!(request.path, "/models/gemini-2.5-flash:streamGenerateContent?alt=sse");
    assert_eq!(request.headers["x-goog-api-key"], "gem-key");
    assert_eq!(request.headers["user-agent"], crate::identity::format_user_agent(None));
    let body = request.json();
    assert_eq!(body["systemInstruction"], json!({"role": "user", "parts": [{"text": "Be brief"}]}));
    assert_eq!(body["contents"], json!([{"role": "user", "parts": [{"text": "hi"}]}]));
    assert_eq!(
        body["generationConfig"],
        json!({"temperature": 0.5, "maxOutputTokens": 4096, "thinkingConfig": {"includeThoughts": true, "thinkingBudget": 8192}})
    );
}

#[tokio::test]
async fn tool_calls_keep_signatures_and_map_stop_to_tool_use() {
    let server = MockHttp::start(vec![Reply::sse(vec![
        parts(json!([{"text": "Reading"}])),
        parts(json!([
            {"functionCall": {"id": "call-1", "name": "read", "args": {"path": "a.txt"}}, "thoughtSignature": "c2ln"},
            {"functionCall": {"id": "call-1", "name": "read", "args": {"path": "b.txt"}}},
            {"functionCall": {"name": "ls", "args": {}}}
        ])),
        finish("STOP"),
    ])])
    .await;
    let tool = Tool::new("read", "Read a file", json!({"type": "object", "properties": {"path": {"type": "string"}}}));
    let context = Context { tools: vec![tool], ..context() };
    let model = model(&server.base_url, "gemini-2.5-flash");
    let options = StreamOptions { tool_choice: Some("any".into()), ..options() };
    let (events, message) = collect(GoogleGenerativeAI.stream(&model, &context, options)).await;
    assert_eq!(
        kinds(&events)[1..7],
        ["text_start", "text_delta", "text_end", "toolcall_start", "toolcall_delta", "toolcall_end"]
    );
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!((calls[0].id.as_str(), calls[0].thought_signature.as_deref()), ("call-1", Some("c2ln")));
    assert_eq!(calls[0].arguments["path"], "a.txt");
    assert!(calls[1].id.starts_with("read_") && calls[1].id != "call-1");
    assert!(calls[2].id.starts_with("ls_"));
    let body = server.request(0).json();
    let declarations = &body["tools"][0]["functionDeclarations"];
    assert_eq!(declarations[0]["parametersJsonSchema"]["properties"]["path"]["type"], "string");
    assert_eq!(body["toolConfig"], json!({"functionCallingConfig": {"mode": "ANY"}}));
    assert_eq!(body["generationConfig"]["thinkingConfig"], json!({"thinkingBudget": 0}));
}

#[tokio::test]
async fn max_tokens_with_a_tool_call_stays_length() {
    let server = MockHttp::start(vec![Reply::sse(vec![
        parts(json!([{"functionCall": {"id": "c", "name": "echo", "args": {"value": "truncated"}}}])),
        finish("MAX_TOKENS"),
    ])])
    .await;
    let message = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
    let (_, message) = collect(message).await;
    assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (StopReason::Length, Some("MAX_TOKENS")));
}

#[tokio::test]
async fn safety_and_malformed_finish_reasons_are_errors_with_raw_reason() {
    for reason in ["MALFORMED_FUNCTION_CALL", "SAFETY"] {
        let server = MockHttp::start(vec![Reply::sse(vec![finish(reason)])]).await;
        let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
        let (_, message) = collect(stream).await;
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.raw_stop_reason.as_deref(), Some(reason));
        assert_eq!(message.error_message, Some(format!("Provider stopped with: {reason}")));
    }
    let server = MockHttp::start(vec![Reply::sse(vec![parts(json!([{"text": "cut"}]))])]).await;
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
    let (_, message) = collect(stream).await;
    assert_eq!(message.error_message.as_deref(), Some("Google stream ended without a finish reason"));
    assert_eq!(message.text(), "cut");
}

#[tokio::test]
async fn http_errors_use_the_sdk_body_format() {
    let body = json!({"error": {"code": 400, "message": "Bad schema", "status": "INVALID_ARGUMENT"}});
    let server = MockHttp::start(vec![Reply::json(400, &body)]).await;
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
    let (events, message) = collect(stream).await;
    assert_eq!(kinds(&events), ["error"]);
    assert_eq!(message.error_message, Some(body.to_string()));

    let server = MockHttp::start(vec![Reply::text(502, "text/plain", "upstream down")]).await;
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
    let (_, message) = collect(stream).await;
    let expected = json!({"error": {"message": "upstream down", "code": 502, "status": "Bad Gateway"}});
    assert_eq!(message.error_message, Some(expected.to_string()));
}

#[tokio::test]
async fn retries_retryable_statuses_only_when_requested() {
    let busy = json!({"error": {"code": 429, "message": "busy", "status": "RESOURCE_EXHAUSTED"}});
    let server = MockHttp::start(vec![
        Reply::json(429, &busy),
        Reply::sse(vec![parts(json!([{"text": "ok"}])), finish("STOP")]),
    ])
    .await;
    let retrying = StreamOptions { max_retries: Some(1), ..options() };
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), retrying);
    let (_, message) = collect(stream).await;
    assert_eq!((message.stop_reason, message.text().as_str()), (StopReason::Stop, "ok"));
    assert_eq!(server.requests().len(), 2);

    let server = MockHttp::start(vec![Reply::json(429, &busy)]).await;
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options());
    let (_, message) = collect(stream).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn payload_hook_and_stream_events_see_sdk_shapes() {
    let server = MockHttp::start(vec![Reply::sse(vec![parts(json!([{"text": "hi"}])), finish("STOP")])]).await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let payloads = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (seen_hook, payload_hook) = (seen.clone(), payloads.clone());
    let options = StreamOptions {
        on_payload: Some(crate::registry::payload_hook(move |payload: &Value, _: &Model| {
            payload_hook.lock().unwrap().push(payload.clone());
            let mut next = payload.clone();
            next["config"]["topK"] = json!(5);
            Some(next)
        })),
        on_provider_stream_event: Some(Arc::new(move |event: &Value, _: &Model| {
            seen_hook.lock().unwrap().push(event.clone())
        })),
        ..options()
    };
    let (_, message) =
        collect(GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options)).await;
    assert_eq!(message.text(), "hi");
    let payload = payloads.lock().unwrap()[0].clone();
    assert_eq!(payload["model"], "gemini-2.5-flash");
    assert_eq!(payload["config"]["systemInstruction"], "Be brief");
    assert_eq!(payload["config"]["maxOutputTokens"], 4096);
    assert_eq!(server.request(0).json()["generationConfig"]["topK"], 5);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0]["candidates"][0]["content"]["parts"][0]["text"], "hi");
}

#[tokio::test]
async fn sends_user_images_as_inline_data() {
    let server = MockHttp::start(vec![Reply::sse(vec![finish("STOP")])]).await;
    let user = Message::User(UserMessage {
        content: UserContent::Blocks(vec![
            ContentBlock::text("look"),
            ContentBlock::Image { data: "aGVsbG8=".into(), mime_type: "image/png".into() },
        ]),
        timestamp: now_ms(),
    });
    let context = Context { messages: vec![user], ..Context::default() };
    let (_, message) =
        collect(GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context, options())).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let body = server.request(0).json();
    assert_eq!(body["contents"][0]["parts"][1], json!({"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}));
    assert!(body.get("systemInstruction").is_none());
}

#[tokio::test]
async fn aborts_mid_stream_and_requires_an_api_key() {
    let server = MockHttp::start(vec![crate::providers::mock_http::Reply {
        body: crate::providers::mock_http::Body::ChunksThenHang(vec![parts(json!([{"text": "partial"}]))]),
        ..Reply::sse(Vec::new())
    }])
    .await;
    let options = options();
    let cancel = options.cancel.clone();
    let stream = GoogleGenerativeAI.stream(&model(&server.base_url, "gemini-2.5-flash"), &context(), options);
    let message = cancel_after_first_text(stream, cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.text(), "partial");

    let options = StreamOptions { api_key: None, ..StreamOptions::default() };
    let (_, message) =
        collect(GoogleGenerativeAI.stream(&model("http://127.0.0.1:9", "gemini-2.5-flash"), &context(), options)).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: google"));
}
