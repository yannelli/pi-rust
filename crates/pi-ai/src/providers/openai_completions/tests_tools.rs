//! Constrained tool sampling and Copilot headers for Chat Completions: strict and grammar tool
//! declarations, grammar call replay and streaming, and the Copilot dynamic headers.

use serde_json::{Value, json};

use super::OpenAICompletions;
use super::test_support::{
    API, assistant, capture, catalog, chunk, done, finish, ok_frames, options, tool_call, user_context,
};
use crate::providers::mock_server::{MockResponse, MockServer, collect, kinds, test_model};
use crate::registry::ApiProvider;
use crate::stream::AssistantMessageEvent;
use crate::types::{ContentBlock, Context, Message, Model, StopReason, Tool, UserContent, UserMessage};

fn ping(constrained: Value) -> Tool {
    let parameters = json!({"type": "object", "properties": {"required": {"type": "string"},
        "optional": {"type": "string"}}, "required": ["required"]});
    Tool { constrained_sampling: Some(constrained), ..Tool::new("ping", "Ping tool", parameters) }
}

fn sql() -> Tool {
    let parameters = json!({"type": "object", "properties": {"input": {"type": "string"}}, "required": ["input"]});
    let constrained = json!({"type": "grammar", "variants": {"openai_lark": "start: /[A-Z 0-9]+/"}});
    Tool { constrained_sampling: Some(constrained), ..Tool::new("sql", "Run SQL", parameters) }
}

fn local(compat: Value) -> Model {
    let mut model = test_model(API, "local-vllm", "http://127.0.0.1:9/v1");
    model.compat = Some(compat);
    model
}

fn with_tool(tool: Tool) -> Context {
    Context { tools: vec![tool], ..user_context("Call ping") }
}

#[tokio::test]
async fn strict_tools_follow_constrained_sampling_and_server_support() {
    let prefer = with_tool(ping(json!({"type": "json_schema", "strict": "prefer"})));
    let body = capture(&local(json!({})), &prefer, options()).await;
    let function = &body["tools"][0]["function"];
    assert!(function.get("strict").is_none(), "unknown endpoints default to non-strict tools");
    assert_eq!(function["parameters"]["required"], json!(["required"]));
    let body = capture(&catalog("groq", "openai/gpt-oss-20b"), &prefer, options()).await;
    let function = &body["tools"][0]["function"];
    assert_eq!(function["strict"], true);
    assert_eq!(function["parameters"]["required"], json!(["required", "optional"]));
    assert_eq!(function["parameters"]["additionalProperties"], false);
}

#[tokio::test]
async fn required_strict_sampling_fails_without_server_support() {
    let require = with_tool(ping(json!({"type": "json_schema", "strict": "require"})));
    let model = local(json!({}));
    let (events, message) = collect(OpenAICompletions.stream(&model, &require, options())).await;
    assert_eq!(kinds(&events), ["error"]);
    let error = message.error_message.unwrap();
    assert!(error.starts_with("Tool \"ping\" requires JSON-schema constrained sampling"), "{error}");
}

#[tokio::test]
async fn grammar_tools_are_custom_tools_and_replay_as_custom_calls() {
    let model = local(json!({"supportsOpenAIGrammarTools": true}));
    let call = ContentBlock::ToolCall(tool_call("call_1", "sql", json!({"input": "SELECT 1"})));
    let messages = vec![Message::user("query"), assistant(&model, vec![call], StopReason::ToolUse)];
    let context = Context { messages, tools: vec![sql()], ..Context::default() };
    let body = capture(&model, &context, options()).await;
    let custom = json!({"name": "sql", "description": "Run SQL",
        "format": {"type": "grammar", "grammar": {"syntax": "lark", "definition": "start: /[A-Z 0-9]+/"}}});
    assert_eq!(body["tools"][0], json!({"type": "custom", "custom": custom}));
    let replayed = body["messages"].as_array().unwrap().iter().find(|message| message["role"] == "assistant");
    let expected = json!({"id": "call_1", "type": "custom", "custom": {"name": "sql", "input": "SELECT 1"}});
    assert_eq!(replayed.unwrap()["tool_calls"][0], expected);
    let fallback = capture(&local(json!({})), &context, options()).await;
    assert_eq!(fallback["tools"][0]["type"], "function");
}

#[tokio::test]
async fn grammar_calls_stream_raw_input_as_append_only_json() {
    let custom = |id: Option<&str>, input: &str| {
        let mut call = json!({"index": 0, "type": "custom", "custom": {"input": input}});
        if let Some(id) = id {
            call["id"] = json!(id);
            call["custom"]["name"] = json!("sql");
        }
        chunk(json!({"tool_calls": [call]}))
    };
    let frames = vec![custom(Some("call_1"), "SELECT"), custom(None, " 1"), finish("tool_calls"), done()];
    let server = MockServer::start(MockResponse::Sse(frames)).await;
    let model = Model { base_url: server.base_url.clone(), ..local(json!({"supportsOpenAIGrammarTools": true})) };
    let (events, message) = collect(OpenAICompletions.stream(&model, &with_tool(sql()), options())).await;
    let deltas: Vec<String> = events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::ToolcallDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, ["{\"input\":\"SELECT", " 1", "\"}"]);
    assert_eq!(serde_json::from_str::<Value>(&deltas.concat()).unwrap(), json!({"input": "SELECT 1"}));
    let calls: Vec<_> = message.tool_calls().collect();
    assert_eq!((calls[0].id.as_str(), calls[0].name.as_str()), ("call_1", "sql"));
    assert_eq!(Value::Object(calls[0].arguments.clone()), json!({"input": "SELECT 1"}));
}

#[tokio::test]
async fn copilot_models_send_dynamic_headers_below_request_headers() {
    let mut server = MockServer::sequence(vec![MockResponse::Sse(ok_frames()), MockResponse::Sse(ok_frames())]).await;
    let mut model = test_model(API, "github-copilot", &server.base_url);
    model.headers.insert("Openai-Intent".into(), "model-default".into());
    let image = ContentBlock::Image { data: "AAAA".into(), mime_type: "image/png".into() };
    let user = Message::User(UserMessage { content: UserContent::Blocks(vec![image]), timestamp: 1 });
    let context = Context { messages: vec![user], ..Context::default() };
    collect(OpenAICompletions.stream(&model, &context, options())).await;
    let headers = server.request().await.headers;
    assert_eq!(headers["x-initiator"], "user");
    assert_eq!(headers["openai-intent"], "conversation-edits");
    assert_eq!(headers["copilot-vision-request"], "true");
    let mut options = options();
    options.headers.insert("X-Initiator".into(), "agent".into());
    collect(OpenAICompletions.stream(&model, &user_context("hi"), options)).await;
    let headers = server.request().await.headers;
    assert_eq!(headers["x-initiator"], "agent");
    assert!(!headers.contains_key("copilot-vision-request"));
}
