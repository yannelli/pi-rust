use serde_json::{Value, json};

use super::OpenAIResponses;
use crate::providers::mock_server::{
    MockResponse, MockServer, cancel_after_first_text, collect, frame, kinds, test_model,
};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, StopReason, ThinkingLevel, Tool, ToolCall, ToolResultMessage,
    Usage,
};

const API: &str = "openai-responses";

fn event(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_string();
    frame(Some(&kind), &data)
}

fn completed(usage: Value) -> String {
    event(
        json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "usage": usage, "output": []}}),
    )
}

fn context() -> Context {
    Context { messages: vec![Message::user("hi")], ..Context::default() }
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("sk-test".into()), ..StreamOptions::default() }
}

#[tokio::test]
async fn streams_text_with_signature_usage_and_cost() {
    let message_item = json!({"type": "message", "id": "msg_1", "role": "assistant", "content": []});
    let done_item = json!({"type": "message", "id": "msg_1", "phase": "final_answer", "content": [{"type": "output_text", "text": "Hello"}]});
    let mut server = MockServer::start(MockResponse::Sse(vec![
        event(json!({"type": "response.created", "response": {"id": "resp_1"}})),
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": message_item})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hel"})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo"})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": done_item})),
        completed(json!({
            "input_tokens": 100, "input_tokens_details": {"cached_tokens": 30},
            "output_tokens": 50, "output_tokens_details": {"reasoning_tokens": 10}, "total_tokens": 150
        })),
    ]))
    .await;
    let model = test_model(API, "openai", &server.base_url);
    let (events, message) = collect(OpenAIResponses.stream(&model, &context(), options())).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_delta", "text_end", "done"]);
    assert_eq!(message.stop_reason, StopReason::Stop);
    let ContentBlock::Text { text, text_signature } = &message.content[0] else { panic!("text") };
    assert_eq!(text, "Hello");
    assert_eq!(text_signature.as_deref(), Some(r#"{"v":1,"id":"msg_1","phase":"final_answer"}"#));
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!((message.usage.input, message.usage.cache_read, message.usage.output), (70, 30, 50));
    assert_eq!((message.usage.reasoning, message.usage.total_tokens), (Some(10), 150));
    assert!((message.usage.cost.total - (70.0 + 30.0 * 0.5 + 50.0 * 2.0) / 1e6).abs() < 1e-12);
    let request = server.request().await;
    assert_eq!(request.path, "/responses");
    assert_eq!(request.headers["authorization"], "Bearer sk-test");
}

#[tokio::test]
async fn reasoning_summary_and_split_tool_arguments() {
    let reasoning = json!({"type": "reasoning", "id": "rs_1", "summary": []});
    let reasoning_done =
        json!({"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Plan it"}]});
    let call = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": ""});
    let call_done = json!({"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "read", "arguments": "{\"path\":\"a.txt\"}"});
    let output = json!([{"type": "reasoning", "id": "rs_1", "encrypted_content": "ENC"}]);
    let server = MockServer::start(MockResponse::Sse(vec![
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": reasoning})),
        event(json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": "Plan"})),
        event(json!({"type": "response.reasoning_summary_text.delta", "output_index": 0, "delta": " it"})),
        event(json!({"type": "response.output_item.done", "output_index": 0, "item": reasoning_done})),
        event(json!({"type": "response.output_item.added", "output_index": 1, "item": call})),
        event(json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"path\":"})),
        event(json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "\"a.txt\"}"})),
        event(json!({"type": "response.function_call_arguments.done", "output_index": 1, "arguments": "{\"path\":\"a.txt\"}"})),
        event(json!({"type": "response.output_item.done", "output_index": 1, "item": call_done})),
        event(json!({"type": "response.completed", "response": {"id": "resp_2", "status": "completed", "output": output}})),
    ]))
    .await;
    let model = test_model(API, "openai", &server.base_url);
    let (events, message) = collect(OpenAIResponses.stream(&model, &context(), options())).await;
    assert_eq!(
        kinds(&events),
        [
            "start",
            "thinking_start",
            "thinking_delta",
            "thinking_delta",
            "thinking_end",
            "toolcall_start",
            "toolcall_delta",
            "toolcall_delta",
            "toolcall_end",
            "done"
        ]
    );
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    let ContentBlock::Thinking { thinking, thinking_signature, .. } = &message.content[0] else { panic!("thinking") };
    assert_eq!(thinking, "Plan it");
    let signature: Value = serde_json::from_str(thinking_signature.as_deref().unwrap()).unwrap();
    assert_eq!((signature["id"].as_str(), signature["encrypted_content"].as_str()), (Some("rs_1"), Some("ENC")));
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), call.name.as_str()), ("call_1|fc_1", "read"));
    assert_eq!(call.arguments["path"], "a.txt");
}

#[tokio::test]
async fn request_body_converts_history_tools_and_reasoning() {
    let mut server = MockServer::start(MockResponse::Sse(vec![completed(json!({}))])).await;
    let model = test_model(API, "openai", &server.base_url);
    let assistant = AssistantMessage {
        content: vec![
            ContentBlock::Thinking {
                thinking: "plan".into(),
                thinking_signature: Some(r#"{"type":"reasoning","id":"rs_0","summary":[]}"#.into()),
                redacted: false,
            },
            ContentBlock::Text { text: "Reading".into(), text_signature: Some(r#"{"v":1,"id":"msg_0"}"#.into()) },
            ContentBlock::ToolCall(ToolCall {
                id: "call_1|fc_1".into(),
                name: "read".into(),
                arguments: json!({"path": "a"}).as_object().cloned().unwrap(),
                thought_signature: None,
                ..Default::default()
            }),
        ],
        api: API.into(),
        provider: "openai".into(),
        model: model.id.clone(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: 1,
        duration_ms: None,
        ..Default::default()
    };
    let result = ToolResultMessage {
        tool_call_id: "call_1|fc_1".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text("contents")],
        details: None,
        is_error: false,
        timestamp: 2,
        duration_ms: None,
        ..Default::default()
    };
    let context = Context {
        system_prompt: Some("Be brief.".into()),
        messages: vec![Message::user("hi"), Message::Assistant(assistant), Message::ToolResult(result)],
        tools: vec![Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type": "object"}),
            ..Default::default()
        }],
    };
    let options = StreamOptions { reasoning: Some(ThinkingLevel::Xhigh), session_id: Some("s1".into()), ..options() };
    let (_, message) = collect(OpenAIResponses.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = server.request().await;
    assert_eq!(request.headers["session_id"], "s1");
    let body = request.body;
    assert_eq!((body["store"].as_bool(), body["max_output_tokens"].as_u64()), (Some(false), Some(4096)));
    assert_eq!(body["prompt_cache_key"], "s1");
    assert_eq!(body["reasoning"], json!({"effort": "high", "summary": "auto"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0], json!({"role": "developer", "content": "Be brief."}));
    assert_eq!(input[1], json!({"role": "user", "content": [{"type": "input_text", "text": "hi"}]}));
    assert_eq!(input[2]["id"], "rs_0");
    assert_eq!((input[3]["type"].as_str(), input[3]["id"].as_str()), (Some("message"), Some("msg_0")));
    assert_eq!(
        input[4],
        json!({"type": "function_call", "call_id": "call_1", "name": "read", "id": "fc_1", "arguments": "{\"path\":\"a\"}"})
    );
    assert_eq!(input[5], json!({"type": "function_call_output", "call_id": "call_1", "output": "contents"}));
    assert_eq!(
        body["tools"][0],
        json!({"type": "function", "name": "read", "description": "Read", "parameters": {"type": "object"}})
    );
}

#[tokio::test]
async fn missing_api_key_fails_without_a_request() {
    let model = test_model(API, "openai", "http://127.0.0.1:9");
    let (events, message) = collect(OpenAIResponses.stream(&model, &context(), StreamOptions::default())).await;
    assert_eq!(kinds(&events), ["error"]);
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: openai"));
}

#[tokio::test]
async fn http_errors_and_incomplete_responses_are_reported() {
    let server = MockServer::start(MockResponse::Status(500, json!({"error": {"message": "boom"}}).to_string())).await;
    let model = test_model(API, "openai", &server.base_url);
    let (_, message) = collect(OpenAIResponses.stream(&model, &context(), options())).await;
    assert_eq!(message.error_message.as_deref(), Some("Provider HTTP 500: boom"));

    let incomplete = json!({"type": "response.incomplete", "response": {"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}});
    let server = MockServer::start(MockResponse::Sse(vec![event(incomplete)])).await;
    let model = test_model(API, "openai", &server.base_url);
    let (_, message) = collect(OpenAIResponses.stream(&model, &context(), options())).await;
    assert_eq!(message.stop_reason, StopReason::Length);

    let server = MockServer::start(MockResponse::Sse(vec![])).await;
    let model = test_model(API, "openai", &server.base_url);
    let (_, message) = collect(OpenAIResponses.stream(&model, &context(), options())).await;
    assert_eq!(
        message.error_message.as_deref(),
        Some("OpenAI Responses stream ended before a terminal response event")
    );
}

#[tokio::test]
async fn cancellation_mid_stream_aborts() {
    let item = json!({"type": "message", "id": "msg_1", "content": []});
    let server = MockServer::start(MockResponse::SseThenHang(vec![
        event(json!({"type": "response.output_item.added", "output_index": 0, "item": item})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hi"})),
    ]))
    .await;
    let model = test_model(API, "openai", &server.base_url);
    let options = options();
    let cancel = options.cancel.clone();
    let message = cancel_after_first_text(OpenAIResponses.stream(&model, &context(), options), cancel).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);
}
