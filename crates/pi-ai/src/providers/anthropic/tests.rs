use serde_json::{Value, json};

use super::AnthropicMessages;
use crate::providers::mock_server::{
    MockResponse, MockServer, cancel_after_first_text, collect, frame, kinds, test_model,
};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, StopReason, ThinkingLevel, Tool, ToolCall, ToolResultMessage,
    Usage,
};

const API: &str = "anthropic-messages";

fn event(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_string();
    frame(Some(&kind), &data)
}

fn message_start(usage: Value) -> String {
    event(json!({"type": "message_start", "message": {"id": "msg_1", "model": "test-model", "usage": usage}}))
}

fn block_start(index: u64, block: Value) -> String {
    event(json!({"type": "content_block_start", "index": index, "content_block": block}))
}

fn delta(index: u64, delta: Value) -> String {
    event(json!({"type": "content_block_delta", "index": index, "delta": delta}))
}

fn block_stop(index: u64) -> String {
    event(json!({"type": "content_block_stop", "index": index}))
}

fn message_end(reason: &str, usage: Value) -> Vec<String> {
    vec![
        event(json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": usage})),
        event(json!({"type": "message_stop"})),
    ]
}

fn context() -> Context {
    Context { messages: vec![Message::user("hi")], ..Context::default() }
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("sk-ant-api-test".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_text_with_usage_and_cost() {
    let mut frames = vec![
        message_start(
            json!({"input_tokens": 100, "output_tokens": 1, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 10}),
        ),
        event(json!({"type": "ping"})),
        block_start(0, json!({"type": "text", "text": ""})),
        delta(0, json!({"type": "text_delta", "text": "Hel"})),
        delta(0, json!({"type": "text_delta", "text": "lo"})),
        block_stop(0),
    ];
    frames.extend(message_end("end_turn", json!({"output_tokens": 25})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model(API, "anthropic", &server.base_url);
    let (events, message) = collect(AnthropicMessages.stream(&model, &context(), options())).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]);
    assert_eq!((message.text(), message.stop_reason), ("Hello".to_string(), StopReason::Stop));
    assert_eq!(message.response_id.as_deref(), Some("msg_1"));
    let usage = message.usage;
    assert_eq!(
        (usage.input, usage.output, usage.cache_read, usage.cache_write, usage.total_tokens),
        (100, 25, 50, 10, 185)
    );
    assert!((usage.cost.total - (100.0 + 25.0 * 2.0 + 50.0 * 0.5 + 10.0 * 4.0) / 1e6).abs() < 1e-12);
    let request = server.request().await;
    assert_eq!(request.path, "/v1/messages");
    assert_eq!(request.headers["x-api-key"], "sk-ant-api-test");
    assert_eq!(request.headers["anthropic-version"], "2023-06-01");
    assert!(!request.headers.contains_key("authorization"));
}

#[tokio::test]
async fn thinking_signature_redacted_thinking_and_tool_arguments() {
    let mut frames = vec![
        message_start(json!({"input_tokens": 10, "output_tokens": 0})),
        block_start(0, json!({"type": "thinking", "thinking": "", "signature": ""})),
        delta(0, json!({"type": "thinking_delta", "thinking": "Need a file."})),
        delta(0, json!({"type": "signature_delta", "signature": "sig-"})),
        delta(0, json!({"type": "signature_delta", "signature": "123"})),
        block_stop(0),
        block_start(1, json!({"type": "redacted_thinking", "data": "opaque"})),
        block_stop(1),
        block_start(2, json!({"type": "tool_use", "id": "toolu_1", "name": "read", "input": {}})),
        delta(2, json!({"type": "input_json_delta", "partial_json": "{\"path\": \"a"})),
        delta(2, json!({"type": "input_json_delta", "partial_json": ".txt\"}"})),
        block_stop(2),
    ];
    frames.extend(message_end(
        "tool_use",
        json!({"output_tokens": 30, "output_tokens_details": {"thinking_tokens": 12}}),
    ));
    let server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model(API, "anthropic", &server.base_url);
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.usage.reasoning, Some(12));
    let ContentBlock::Thinking { thinking, thinking_signature, redacted } = &message.content[0] else {
        panic!("thinking")
    };
    assert_eq!((thinking.as_str(), thinking_signature.as_deref(), *redacted), ("Need a file.", Some("sig-123"), false));
    let ContentBlock::Thinking { thinking_signature, redacted, .. } = &message.content[1] else { panic!("redacted") };
    assert_eq!((thinking_signature.as_deref(), *redacted), (Some("opaque"), true));
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), call.arguments["path"].as_str()), ("toolu_1", Some("a.txt")));
}

fn history(model_id: &str) -> Vec<Message> {
    let assistant = AssistantMessage {
        content: vec![
            ContentBlock::Thinking { thinking: "plan".into(), thinking_signature: Some("sig".into()), redacted: false },
            ContentBlock::Thinking {
                thinking: "[Reasoning redacted]".into(),
                thinking_signature: Some("opaque".into()),
                redacted: true,
            },
            ContentBlock::text("Reading"),
            ContentBlock::ToolCall(ToolCall {
                id: "call|1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.png"}).as_object().cloned().unwrap(),
                thought_signature: None,
                ..Default::default()
            }),
        ],
        api: API.into(),
        provider: "anthropic".into(),
        model: model_id.into(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1,
        duration_ms: None,
        ..Default::default()
    };
    let result = ToolResultMessage {
        tool_call_id: "call|1".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() }],
        details: None,
        is_error: false,
        timestamp: 2,
        duration_ms: None,
        ..Default::default()
    };
    vec![Message::user("hi"), Message::Assistant(assistant), Message::ToolResult(result)]
}

fn tools() -> Vec<Tool> {
    let parameters = json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]});
    vec![Tool { name: "read".into(), description: "Read a file".into(), parameters, ..Default::default() }]
}

#[tokio::test]
async fn request_body_caches_and_replays_thinking_with_budget() {
    let mut frames = vec![message_start(json!({"input_tokens": 1}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 1})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = test_model(API, "anthropic", &server.base_url);
    let context = Context { system_prompt: Some("Be brief.".into()), messages: history(&model.id), tools: tools() };
    let options = StreamOptions { reasoning: Some(ThinkingLevel::Medium), temperature: Some(0.5), ..options() };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = server.request().await;
    assert!(request.headers["anthropic-beta"].contains("interleaved-thinking-2025-05-14"));
    let body = request.body;
    let ephemeral = json!({"type": "ephemeral"});
    assert_eq!(body["system"], json!([{"type": "text", "text": "Be brief.", "cache_control": ephemeral}]));
    assert_eq!(body["thinking"], json!({"type": "enabled", "budget_tokens": 3072, "display": "summarized"}));
    assert_eq!(body["max_tokens"], 4096);
    assert!(body.get("temperature").is_none(), "temperature is incompatible with thinking");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0], json!({"role": "user", "content": "hi"}));
    let assistant = messages[1]["content"].as_array().unwrap();
    assert_eq!(assistant[0], json!({"type": "thinking", "thinking": "plan", "signature": "sig"}));
    assert_eq!(assistant[1], json!({"type": "redacted_thinking", "data": "opaque"}));
    assert_eq!(assistant[3], json!({"type": "tool_use", "id": "call|1", "name": "read", "input": {"path": "a.png"}}));
    let result = &messages[2]["content"][0];
    assert_eq!((result["type"].as_str(), result["tool_use_id"].as_str()), (Some("tool_result"), Some("call|1")));
    assert_eq!(result["content"][0]["text"], "(see attached image)");
    assert_eq!(result["content"][1]["source"]["media_type"], "image/png");
    assert_eq!(result["cache_control"], ephemeral);
    let tool = &body["tools"][0];
    assert_eq!(
        tool["input_schema"],
        json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
    );
    assert_eq!((tool["eager_input_streaming"].as_bool(), &tool["cache_control"]), (Some(true), &ephemeral));
}

#[tokio::test]
async fn oauth_tokens_use_bearer_auth_claude_code_identity_and_adaptive_thinking() {
    let mut frames = vec![
        message_start(json!({"input_tokens": 1})),
        block_start(0, json!({"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "x"}})),
        block_stop(0),
    ];
    frames.extend(message_end("tool_use", json!({"output_tokens": 1})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    let mut model = test_model(API, "anthropic", &server.base_url);
    model.compat = Some(json!({"forceAdaptiveThinking": true}));
    let context =
        Context { system_prompt: Some("Be brief.".into()), messages: vec![Message::user("hi")], tools: tools() };
    let options = StreamOptions {
        api_key: Some("sk-ant-oat01-token".into()),
        reasoning: Some(ThinkingLevel::Low),
        ..StreamOptions::default()
    };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context, options)).await;
    assert_eq!(message.tool_calls().next().unwrap().name, "read");
    let request = server.request().await;
    assert_eq!(request.headers["authorization"], "Bearer sk-ant-oat01-token");
    assert!(!request.headers.contains_key("x-api-key"));
    assert_eq!(request.headers["anthropic-beta"], "claude-code-20250219,oauth-2025-04-20");
    let body = request.body;
    assert_eq!(body["system"][0]["text"], "You are Claude Code, Anthropic's official CLI for Claude.");
    assert_eq!(body["system"][1]["text"], "Be brief.");
    assert_eq!(body["tools"][0]["name"], "Read");
    assert_eq!(body["thinking"], json!({"type": "adaptive", "display": "summarized"}));
    assert_eq!(body["output_config"], json!({"effort": "low"}));
}

#[tokio::test]
async fn errors_missing_key_and_truncated_streams() {
    let body = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}).to_string();
    let server = MockServer::start(MockResponse::Status(529, body)).await;
    let model = test_model(API, "anthropic", &server.base_url);
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Provider HTTP 529: Overloaded"));

    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), StreamOptions::default())).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: anthropic"));

    let server = MockServer::start(MockResponse::Sse(vec![message_start(json!({"input_tokens": 1}))])).await;
    let model = test_model(API, "anthropic", &server.base_url);
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Anthropic stream ended before message_stop"));
}

#[tokio::test]
async fn cancellation_mid_stream_aborts() {
    let server = MockServer::start(MockResponse::SseThenHang(vec![
        message_start(json!({"input_tokens": 1})),
        block_start(0, json!({"type": "text", "text": ""})),
        delta(0, json!({"type": "text_delta", "text": "Hi"})),
    ]))
    .await;
    let model = test_model(API, "anthropic", &server.base_url);
    let options = options();
    let cancel = options.cancel.clone();
    let message = cancel_after_first_text(AnthropicMessages.stream(&model, &context(), options), cancel).await;
    assert_eq!((message.stop_reason, message.text()), (StopReason::Aborted, "Hi".to_string()));
}
