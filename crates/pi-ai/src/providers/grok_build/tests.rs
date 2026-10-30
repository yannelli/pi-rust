use serde_json::{Value, json};

use super::{GROK_CLIENT_VERSION, GrokBuildResponses, rewrite_body};
use crate::providers::mock_server::{MockResponse, MockServer, collect, frame, kinds, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, StopReason, Tool};

const API: &str = "grok-build-responses";

fn event(data: Value) -> String {
    let kind = data["type"].as_str().unwrap().to_string();
    frame(Some(&kind), &data)
}

fn options() -> StreamOptions {
    StreamOptions { api_key: Some("grok-token".into()), session_id: Some("conv-1".into()), ..StreamOptions::default() }
}

fn hello_stream() -> MockResponse {
    MockResponse::Sse(vec![
        event(json!({"type": "response.created", "response": {"id": "resp_1"}})),
        event(json!({"type": "response.output_item.added", "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "role": "assistant", "content": []}})),
        event(json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hi"})),
        event(json!({"type": "response.output_item.done", "output_index": 0,
            "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "Hi"}]}})),
        event(json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed",
            "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12}, "output": []}})),
    ])
}

#[tokio::test]
async fn sends_grok_cli_headers_and_streams_text() {
    let mut server = MockServer::start(hello_stream()).await;
    let model = test_model(API, "grok-build", &server.base_url);
    let context =
        Context { system_prompt: Some("Be brief".into()), messages: vec![Message::user("hi")], ..Context::default() };
    let (events, message) = collect(GrokBuildResponses.stream(&model, &context, options())).await;
    assert_eq!(kinds(&events), ["start", "text_start", "text_delta", "text_end", "done"]);
    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = server.request().await;
    assert_eq!(request.path, "/responses");
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    assert_eq!(header("authorization"), Some("Bearer grok-token"));
    assert_eq!(header("x-xai-token-auth"), Some("xai-grok-cli"));
    assert_eq!(header("x-grok-client-version"), Some(GROK_CLIENT_VERSION));
    assert_eq!(header("x-grok-client-identifier"), Some("grok-shell"));
    assert_eq!(header("x-grok-client-mode"), Some("cli"));
    assert_eq!(header("x-grok-model-override"), Some("test-model"));
    assert_eq!(header("x-grok-conv-id"), Some("conv-1"));
    assert_eq!(header("user-agent"), Some(format!("grok-shell/{GROK_CLIENT_VERSION} (pi)").as_str()));
    assert_eq!(request.body["instructions"], "Be brief");
    assert_eq!(request.body["store"], false);
    assert_eq!(request.body["include"], json!(["reasoning.encrypted_content"]));
    assert!(request.body.get("max_output_tokens").is_none());
    let roles: Vec<&str> =
        request.body["input"].as_array().unwrap().iter().filter_map(|item| item["role"].as_str()).collect();
    assert_eq!(roles, ["user"]);
}

#[tokio::test]
async fn replayed_items_lose_ids_and_tools_are_not_strict() {
    let mut server = MockServer::start(hello_stream()).await;
    let model = test_model(API, "grok-build", &server.base_url);
    let assistant = AssistantMessage {
        content: vec![ContentBlock::Text {
            text: "Earlier".into(),
            text_signature: Some(r#"{"v":1,"id":"msg_9"}"#.into()),
        }],
        api: API.into(),
        provider: "grok-build".into(),
        model: "test-model".into(),
        ..AssistantMessage::default()
    };
    let tool = Tool {
        name: "noop".into(),
        description: "No input".into(),
        parameters: json!({"type": "object"}),
        ..Tool::default()
    };
    let context = Context {
        messages: vec![Message::user("hi"), Message::Assistant(assistant), Message::user("again")],
        tools: vec![tool],
        ..Context::default()
    };
    let options = StreamOptions { temperature: Some(0.3), ..options() };
    collect(GrokBuildResponses.stream(&model, &context, options)).await;
    let body = server.request().await.body;
    assert!(body["input"].as_array().unwrap().iter().all(|item| item.get("id").is_none()));
    assert!(body.get("temperature").is_none());
    assert_eq!(body["tools"][0]["strict"], false);
    assert_eq!(body["tools"][0]["parameters"]["properties"], json!({}));
    assert_eq!(body["tool_choice"], "auto");
}

#[test]
fn rewrite_folds_instructions_and_drops_unreplayable_items() {
    let body = json!({
        "instructions": "First",
        "input": [
            {"role": "developer", "content": [{"type": "input_text", "text": "Second"}]},
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "reasoning", "id": "rs_2", "encrypted_content": "blob", "summary": []},
            {"role": "user", "content": [{"type": "input_text", "text": "  "}]},
            {"role": "user", "content": [{"type": "input_text", "text": ""}, {"type": "input_image", "image_url": "x"}]},
        ],
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_retention": "24h",
        "top_p": 0.9,
        "reasoning": {"effort": "high", "summary": "auto"},
    });
    let rewritten = rewrite_body(body);
    assert_eq!(rewritten["instructions"], "First\n\nSecond");
    assert_eq!(
        rewritten["input"],
        json!([
            {"type": "reasoning", "encrypted_content": "blob", "summary": []},
            {"role": "user", "content": [{"type": "input_image", "image_url": "x"}]},
        ])
    );
    assert_eq!(rewritten["include"], json!(["reasoning.encrypted_content"]));
    assert!(rewritten.get("prompt_cache_retention").is_none() && rewritten.get("top_p").is_none());
    assert!(rewritten.get("tools").is_none() && rewritten.get("tool_choice").is_none());
    assert_eq!(rewritten["reasoning"], json!({"effort": "high"}));
    let off = rewrite_body(json!({"input": [], "reasoning": {"effort": "none"}}));
    assert!(off.get("reasoning").is_none());
}

#[tokio::test]
async fn client_version_rejection_names_the_override() {
    let server = MockServer::start(MockResponse::Status(426, json!({"error": "upgrade required"}).to_string())).await;
    let model = test_model(API, "grok-build", &server.base_url);
    let mut options = options();
    options.env.insert("GROK_CLIENT_VERSION".into(), "0.9.0".into());
    let context = Context { messages: vec![Message::user("hi")], ..Context::default() };
    let (_, message) = collect(GrokBuildResponses.stream(&model, &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.unwrap();
    assert!(error.starts_with("Provider HTTP 426"), "{error}");
    assert!(error.contains("rejected client version 0.9.0"), "{error}");
}
