//! Streaming basics, request conversion, and HTTP failures.

use serde_json::json;

use super::OpenAICompletions;
use super::test_support::{API, chunk, done, finish, options};
use crate::providers::mock_server::{
    MockResponse, MockServer, cancel_after_first_text, collect, frame, kinds, test_model,
};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, StopReason, ThinkingLevel, Tool, ToolCall, ToolResultMessage,
    Usage, UserContent, UserMessage,
};

fn context() -> Context {
    Context { messages: vec![Message::user("hi")], ..Context::default() }
}

#[tokio::test]
async fn streams_text_with_usage_and_cost() {
    let usage = json!({"id": "chatcmpl-1", "choices": [], "usage": {
        "prompt_tokens": 100, "completion_tokens": 20,
        "prompt_tokens_details": {"cached_tokens": 40}, "completion_tokens_details": {"reasoning_tokens": 5}
    }});
    let mut server = MockServer::start(MockResponse::Sse(vec![
        chunk(json!({"role": "assistant", "content": "Hel"})),
        chunk(json!({"content": "lo"})),
        finish("stop"),
        frame(None, &usage),
        done(),
    ]))
    .await;
    let model = test_model(API, "local", &server.base_url);
    let (events, message) = collect(OpenAICompletions.stream(&model, &context(), options())).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]);
    assert_eq!(message.text(), "Hello");
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(message.response_id.as_deref(), Some("chatcmpl-1"));
    assert_eq!((message.usage.input, message.usage.cache_read, message.usage.output), (60, 40, 20));
    assert_eq!((message.usage.reasoning, message.usage.total_tokens), (Some(5), 120));
    assert!((message.usage.cost.total - (60.0 + 40.0 * 0.5 + 20.0 * 2.0) / 1e6).abs() < 1e-12);
    let request = server.request().await;
    assert_eq!(request.path, "/chat/completions");
    assert_eq!(request.headers["authorization"], "Bearer test-key");
    assert_eq!(request.headers["user-agent"], crate::http::pi_user_agent());
}

#[tokio::test]
async fn interleaved_tool_calls_are_assembled_by_index() {
    let call = |index: u64, id: Option<&str>, name: Option<&str>, arguments: &str| {
        let mut function = json!({"arguments": arguments});
        if let Some(name) = name {
            function["name"] = json!(name);
        }
        let mut call = json!({"index": index, "type": "function", "function": function});
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        chunk(json!({"tool_calls": [call]}))
    };
    let server = MockServer::start(MockResponse::Sse(vec![
        call(0, Some("call_a"), Some("read"), "{\"pa"),
        call(1, Some("call_b"), Some("ls"), "{\"dir"),
        call(0, None, None, "th\":\"a.txt\"}"),
        call(1, None, None, "\":\"src\"}"),
        finish("tool_calls"),
        done(),
    ]))
    .await;
    let model = test_model(API, "local", &server.base_url);
    let (events, message) = collect(OpenAICompletions.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!(calls.len(), 2);
    assert_eq!((calls[0].id.as_str(), calls[0].name.as_str()), ("call_a", "read"));
    assert_eq!(calls[0].arguments["path"], "a.txt");
    assert_eq!((calls[1].id.as_str(), calls[1].name.as_str()), ("call_b", "ls"));
    assert_eq!(calls[1].arguments["dir"], "src");
    assert_eq!(kinds(&events).iter().filter(|kind| *kind == "toolcall_end").count(), 2);
}

#[tokio::test]
async fn reasoning_content_becomes_signed_thinking() {
    let server = MockServer::start(MockResponse::Sse(vec![
        chunk(json!({"reasoning_content": "Let me "})),
        chunk(json!({"reasoning_content": "think."})),
        chunk(json!({"content": "Answer"})),
        finish("stop"),
        done(),
    ]))
    .await;
    let model = test_model(API, "local", &server.base_url);
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options())).await;
    let ContentBlock::Thinking { thinking, thinking_signature, .. } = &message.content[0] else { panic!("thinking") };
    assert_eq!(thinking, "Let me think.");
    assert_eq!(thinking_signature.as_deref(), Some("reasoning_content"));
    assert_eq!(message.text(), "Answer");
}

fn history(model_id: &str) -> Vec<Message> {
    let call = ToolCall {
        id: "call_1".into(),
        name: "read".into(),
        arguments: json!({"path": "a.png"}).as_object().cloned().unwrap(),
        thought_signature: None,
        ..Default::default()
    };
    vec![
        Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::text("Look"),
                ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() },
            ]),
            timestamp: 1,
        }),
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking {
                    thinking: "hmm".into(),
                    thinking_signature: Some("reasoning_content".into()),
                    redacted: false,
                },
                ContentBlock::text("Reading"),
                ContentBlock::ToolCall(call),
            ],
            api: API.into(),
            provider: "local".into(),
            model: model_id.into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 2,
            duration_ms: None,
            ..Default::default()
        }),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "read".into(),
            content: vec![
                ContentBlock::text("binary"),
                ContentBlock::Image { data: "BBBB".into(), mime_type: "image/png".into() },
            ],
            details: None,
            is_error: false,
            timestamp: 3,
            duration_ms: None,
            ..Default::default()
        }),
    ]
}

#[tokio::test]
async fn request_body_converts_messages_tools_and_options() {
    let mut server =
        MockServer::start(MockResponse::Sse(vec![chunk(json!({"content": "ok"})), finish("stop"), done()])).await;
    let mut model = test_model(API, "local", &server.base_url);
    model.compat = Some(json!({"maxTokensField": "max_tokens"}));
    model.headers.insert("X-Model".into(), "m".into());
    let context = Context {
        system_prompt: Some("Be brief.".into()),
        messages: history(&model.id),
        tools: vec![Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            ..Default::default()
        }],
    };
    let mut options = StreamOptions {
        api_key: Some("sk-test".into()),
        temperature: Some(0.2),
        reasoning: Some(ThinkingLevel::Medium),
        ..StreamOptions::default()
    };
    options.headers.insert("x-model".into(), "override".into());
    let (_, message) = collect(OpenAICompletions.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = server.request().await;
    assert_eq!(request.headers["authorization"], "Bearer sk-test");
    assert_eq!(request.headers["x-model"], "override");
    let body = request.body;
    assert_eq!((body["model"].as_str(), body["stream"].as_bool()), (Some("test-model"), Some(true)));
    assert_eq!(body["max_tokens"], 4096);
    assert!(body.get("max_completion_tokens").is_none());
    assert_eq!((body["temperature"].as_f64(), body["reasoning_effort"].as_str()), (Some(0.2), Some("medium")));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({"role": "developer", "content": "Be brief."}));
    assert_eq!(messages[1]["content"][1]["image_url"]["url"], "data:image/png;base64,AAAA");
    assert_eq!(messages[2]["content"], "Reading");
    assert_eq!(messages[2]["reasoning_content"], "hmm");
    assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"], r#"{"path":"a.png"}"#);
    assert_eq!(messages[3], json!({"role": "tool", "content": "binary", "tool_call_id": "call_1"}));
    assert_eq!(messages[4]["content"][0]["text"], "Attached image(s) from tool result:");
    assert_eq!(messages[4]["content"][1]["image_url"]["url"], "data:image/png;base64,BBBB");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
    assert!(body["tools"][0]["function"].get("strict").is_none());
}

#[tokio::test]
async fn http_errors_report_the_provider_message() {
    let body = json!({"error": {"message": "Invalid API key", "type": "invalid_request_error"}}).to_string();
    let server = MockServer::start(MockResponse::Status(401, body)).await;
    let model = test_model(API, "local", &server.base_url);
    let options = StreamOptions { api_key: Some("bad".into()), ..StreamOptions::default() };
    let (events, message) = collect(OpenAICompletions.stream(&model, &context(), options)).await;
    assert_eq!(kinds(&events), ["error"]);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Provider HTTP 401: Invalid API key"));
}

#[tokio::test]
async fn missing_finish_reason_is_an_error() {
    let server = MockServer::start(MockResponse::Sse(vec![chunk(json!({"content": "partial"})), done()])).await;
    let model = test_model(API, "local", &server.base_url);
    let (_, message) = collect(OpenAICompletions.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("Stream ended without finish_reason"));
    assert_eq!(message.text(), "partial");
}

#[tokio::test]
async fn cancellation_mid_stream_aborts() {
    let server = MockServer::start(MockResponse::SseThenHang(vec![chunk(json!({"content": "Hi"}))])).await;
    let model = test_model(API, "local", &server.base_url);
    let options = options();
    let cancel = options.cancel.clone();
    let message = cancel_after_first_text(OpenAICompletions.stream(&model, &context(), options), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.text(), "Hi");
}
