//! System messages, native tool changes, cache retention, and request options, ported from
//! upstream `transcript-tool-changes`, `cache-retention`, and `anthropic-tool-name-normalization` tests.

use serde_json::{Value, json};

use super::AnthropicMessages;
use super::tests::{API, message_end, message_start, options};
use crate::providers::mock_server::{MockResponse, MockServer, RecordedRequest, capture_payload, collect, test_model};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::types::{AssistantMessage, ContentBlock, Context, Message, Model, StopReason, Tool, ToolCall};

const UNREACHABLE: &str = "http://127.0.0.1:9";

fn tool(name: &str) -> Tool {
    Tool::new(name, format!("{name} tool"), json!({"type": "object", "properties": {}}))
}

fn system(value: Value) -> Message {
    serde_json::from_value(value).unwrap()
}

fn model(compat: Value) -> Model {
    let mut model = test_model(API, "anthropic", UNREACHABLE);
    model.compat = Some(compat);
    model
}

fn native() -> Model {
    model(json!({"supportsMidConvoSystemMessages": true, "supportsMidConvoToolChanges": true}))
}

fn changing_messages() -> Vec<Message> {
    vec![
        system(json!({"role": "system", "content": "base prompt",
            "sections": {"rules": "<rules>\nold rules\n</rules>", "docs": "<docs>\nread docs\n</docs>"},
            "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("before"),
        system(json!({"role": "system", "content": "updated guidance",
            "sections": {"rules": "<rules>\nnew rules\n</rules>", "docs": null},
            "toolsRemoved": [{"name": "base_tool"}], "toolsAdded": [tool("late_tool")], "timestamp": 2})),
    ]
}

fn context(messages: Vec<Message>) -> Context {
    Context { messages, ..Context::default() }
}

async fn payload(model: &Model, messages: Vec<Message>) -> Value {
    capture_payload(&AnthropicMessages, model, &context(messages), options()).await
}

/// Send one request to a mock server and return what it received.
async fn sent(mut model: Model, context: &Context, options: StreamOptions) -> RecordedRequest {
    let mut frames = vec![message_start(json!({"input_tokens": 1}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 1})));
    let mut server = MockServer::start(MockResponse::Sse(frames)).await;
    model.base_url = server.base_url.clone();
    let (_, message) = collect(AnthropicMessages.stream(&model, context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    server.request().await
}

fn names(tools: &Value) -> Vec<&str> {
    tools.as_array().into_iter().flatten().map(|tool| tool["name"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn native_updates_carry_tool_changes_and_the_placeholder() {
    let request = sent(native(), &context(changing_messages()), options()).await;
    assert!(request.headers["anthropic-beta"].contains("inline-tools-2026-09-15"));
    let body = request.body;
    let system: Vec<&str> = body["system"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap()).collect();
    assert_eq!(system, ["base prompt\n\n<rules>\nold rules\n</rules>\n\n<docs>\nread docs\n</docs>"]);
    assert_eq!(names(&body["tools"]), ["base_tool", "__pi_deferred_placeholder__"]);
    assert_eq!(body["tools"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert!(body["tools"][0].get("defer_loading").is_none() && body["tools"][1].get("cache_control").is_none());
    assert_eq!(body["tools"][1]["defer_loading"], true);
    let update = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(update["role"], "system");
    let blocks = update["content"].as_array().unwrap();
    let text = blocks[0]["text"].as_str().unwrap();
    assert!(text.contains("updated guidance") && text.contains("<rules>\nnew rules\n</rules>"));
    assert!(text.contains("Removed system prompt section \"docs\""));
    assert_eq!(blocks[1], json!({"type": "tool_removal", "tool": {"type": "tool_reference", "name": "base_tool"}}));
    assert_eq!(blocks[2]["type"], "tool_addition");
    assert_eq!(blocks[2]["cache_control"], json!({"type": "ephemeral"}));
    let definition = &blocks[2]["tool"]["definition"];
    assert_eq!(
        (definition["name"].as_str(), definition["description"].as_str()),
        (Some("late_tool"), Some("late_tool tool"))
    );
    assert!(definition.get("cache_control").is_none() && definition.get("defer_loading").is_none());
    let initial = payload(&native(), changing_messages()[..2].to_vec()).await;
    assert_eq!(names(&initial["tools"]), ["base_tool", "__pi_deferred_placeholder__"]);
}

#[tokio::test]
async fn redefinitions_replace_tools_by_value() {
    let mut redefined = tool("base_tool");
    redefined.description = "changed".into();
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt", "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("before"),
        system(json!({"role": "system", "content": "", "toolsRemoved": [{"name": "base_tool"}],
            "toolsAdded": [redefined], "timestamp": 2})),
    ];
    let body = payload(&native(), messages).await;
    assert_eq!(body["tools"][0]["description"], "base_tool tool");
    let blocks = body["messages"].as_array().unwrap().last().unwrap()["content"].as_array().unwrap().clone();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["tool"]["definition"]["description"], "changed");
}

#[tokio::test]
async fn without_initial_tools_the_current_list_is_sent() {
    let messages = vec![
        system(json!({"role": "system", "content": "base prompt", "timestamp": 0})),
        Message::user("before"),
        system(
            json!({"role": "system", "content": "updated guidance", "toolsAdded": [tool("late_tool")], "timestamp": 2}),
        ),
    ];
    let request = sent(native(), &context(messages), options()).await;
    assert!(!request.headers.contains_key("anthropic-beta"));
    assert_eq!(names(&request.body["tools"]), ["late_tool"]);
    assert_eq!(request.body["tools"][0]["cache_control"], json!({"type": "ephemeral"}));
    let last = request.body["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        last["content"].as_array().unwrap().iter().map(|b| b["type"].clone()).collect::<Vec<_>>(),
        [json!("text")]
    );
}

#[tokio::test]
async fn updates_fold_into_the_system_prompt_without_both_capabilities() {
    for compat in [json!({}), json!({"supportsMidConvoToolChanges": true})] {
        let body = payload(&model(compat), changing_messages()).await;
        assert_eq!(body["system"][0]["text"], "base prompt\n\nupdated guidance\n\n<rules>\nnew rules\n</rules>");
        assert_eq!(names(&body["tools"]), ["late_tool"]);
        let roles: Vec<&str> =
            body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user"]);
    }
}

fn tool_turn() -> Vec<Message> {
    let call = ToolCall { id: "toolu_1".into(), name: "base_tool".into(), ..Default::default() };
    let assistant = AssistantMessage {
        content: vec![ContentBlock::ToolCall(call)],
        api: API.into(),
        provider: "anthropic".into(),
        model: "test-model".into(),
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    };
    let result = serde_json::from_value(json!({"role": "toolResult", "toolCallId": "toolu_1", "toolName": "base_tool",
        "content": [{"type": "text", "text": "done"}], "isError": false, "timestamp": 4}))
    .unwrap();
    vec![
        system(json!({"role": "system", "content": "base prompt", "toolsAdded": [tool("base_tool")], "timestamp": 0})),
        Message::user("go"),
        Message::Assistant(assistant.clone()),
        system(json!({"role": "system", "content": "mid update", "timestamp": 3})),
        Message::ToolResult(result),
        Message::Assistant(AssistantMessage { content: vec![ContentBlock::text("ok")], ..assistant }),
    ]
}

#[tokio::test]
async fn system_messages_wait_for_the_next_assistant_message() {
    let model = model(json!({"supportsMidConvoSystemMessages": true}));
    let body = payload(&model, tool_turn()).await;
    let roles: Vec<&str> = body["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "system", "assistant"]);
    assert_eq!(body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(body["messages"][3]["content"][0]["text"], "mid update");
}

#[tokio::test]
async fn cache_retention_controls_cache_control_and_affinity() {
    let messages = vec![Message::user("hi")];
    let context = Context { system_prompt: Some("sys".into()), tools: vec![tool("read")], messages };
    let with =
        |retention| StreamOptions { cache_retention: Some(retention), session_id: Some("s1".into()), ..options() };
    let affinity = model(json!({"sendSessionAffinityHeaders": true}));
    let none = sent(affinity.clone(), &context, with(CacheRetention::None)).await;
    assert!(none.body["system"][0].get("cache_control").is_none());
    assert!(none.body["tools"][0].get("cache_control").is_none());
    assert_eq!(none.body["messages"][0]["content"], "hi");
    assert!(!none.headers.contains_key("x-session-affinity"));
    let long = sent(affinity.clone(), &context, with(CacheRetention::Long)).await;
    let ttl = json!({"type": "ephemeral", "ttl": "1h"});
    assert_eq!((&long.body["system"][0]["cache_control"], &long.body["tools"][0]["cache_control"]), (&ttl, &ttl));
    assert_eq!(long.body["messages"][0]["content"][0]["cache_control"], ttl);
    assert_eq!(long.headers["x-session-affinity"], "s1");
    let short = model(json!({"supportsLongCacheRetention": false, "supportsCacheControlOnTools": false}));
    let short = sent(short, &context, with(CacheRetention::Long)).await;
    assert_eq!(short.body["system"][0]["cache_control"], json!({"type": "ephemeral"}));
    assert!(short.body["tools"][0].get("cache_control").is_none());
    let mut openrouter = test_model(API, "openrouter", UNREACHABLE);
    openrouter.compat = None;
    let request = sent(openrouter, &context, with(CacheRetention::Short)).await;
    assert_eq!(request.headers["x-session-id"], "s1");
}

#[tokio::test]
async fn oauth_tool_names_round_trip_and_ids_are_normalized() {
    let mut turn = tool_turn();
    turn.remove(3);
    if let Message::Assistant(assistant) = &mut turn[2] {
        (assistant.provider, assistant.api) = ("openai".into(), "openai-responses".into());
        if let ContentBlock::ToolCall(call) = &mut assistant.content[0] {
            (call.name, call.id) = ("read".into(), "call.1/x".into());
        }
    }
    if let Message::ToolResult(result) = &mut turn[3] {
        result.tool_call_id = "call.1/x".into();
    }
    let oauth = StreamOptions { api_key: Some("sk-ant-oat01-token".into()), ..StreamOptions::default() };
    let mut tools_model = model(json!({}));
    tools_model.reasoning = false;
    let body = capture_payload(&AnthropicMessages, &tools_model, &context(turn), oauth).await;
    assert_eq!(body["messages"][1]["content"][0]["name"], "Read");
    assert_eq!(body["messages"][1]["content"][0]["id"], "call_1_x");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "call_1_x");
    assert_eq!(body["tools"][0]["name"], "base_tool");
}

#[tokio::test]
async fn tool_choice_metadata_and_beta_overrides() {
    let context = Context { messages: vec![Message::user("hi")], tools: vec![tool("read")], ..Context::default() };
    let mut metadata = serde_json::Map::new();
    metadata.insert("user_id".into(), json!("user-1"));
    metadata.insert("other".into(), json!(1));
    let mut lazy = model(json!({"supportsEagerToolInputStreaming": false}));
    lazy.reasoning = false;
    let options = StreamOptions { tool_choice: Some("none".into()), metadata: Some(metadata), ..options() };
    let request = sent(lazy.clone(), &context, options.clone()).await;
    assert_eq!(request.body["tool_choice"], json!({"type": "none"}));
    assert_eq!(request.body["metadata"], json!({"user_id": "user-1"}));
    assert_eq!(request.headers["anthropic-beta"], "fine-grained-tool-streaming-2025-05-14");
    assert!(request.body["tools"][0].get("eager_input_streaming").is_none());
    let mut configured = options.clone();
    configured.headers.insert("Anthropic-Beta".into(), " a, b ,a,".into());
    assert_eq!(sent(lazy.clone(), &context, configured).await.headers["anthropic-beta"], "a,b");
    let suppressed = StreamOptions { suppressed_headers: vec!["anthropic-beta".into()], ..options };
    assert!(!sent(lazy, &context, suppressed).await.headers.contains_key("anthropic-beta"));
}

fn strict_tool(parameters: Value) -> Tool {
    let mut tool = Tool::new("lookup", "Look up a value", parameters);
    tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "prefer"}));
    tool
}

async fn first_tool(tool: Tool) -> Value {
    let model = model(json!({"forceAdaptiveThinking": true, "supportsStrictTools": true}));
    let context = Context { messages: vec![Message::user("Use the tool")], tools: vec![tool], ..Context::default() };
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..options() };
    capture_payload(&AnthropicMessages, &model, &context, options).await["tools"][0].clone()
}

#[tokio::test]
async fn strict_tools_send_the_full_schema_only_when_supported() {
    let legacy = json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"],
        "additionalProperties": false, "title": "LookupInput"});
    let tool = first_tool(Tool::new("lookup", "Look up a value", legacy)).await;
    assert!(tool.get("strict").is_none());
    let expected = json!({"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]});
    assert_eq!(tool["input_schema"], expected);
    let strict = first_tool(strict_tool(json!({"type": "object", "title": "StrictLookupInput",
        "properties": {"value": {"type": "string"}, "optional": {"type": "number"}}, "required": ["value"]})))
    .await;
    assert_eq!(strict["strict"], true);
    assert_eq!(strict["input_schema"]["additionalProperties"], false);
    assert_eq!(strict["input_schema"]["required"], json!(["value", "optional"]));
    assert_eq!(
        strict["input_schema"]["properties"]["optional"],
        json!({"anyOf": [{"type": "number"}, {"type": "null"}]})
    );
    assert_eq!(strict["input_schema"]["title"], "StrictLookupInput");
    for rejected in [
        json!({"timeoutMs": {"type": "integer", "minimum": 1, "maximum": 300000}}),
        json!({"tags": {"type": "array", "items": {"type": "string"}, "minItems": 2}}),
        json!({"expression": {"type": "string", "format": "regex"}}),
    ] {
        let tool = first_tool(strict_tool(json!({"type": "object", "properties": rejected}))).await;
        assert!(tool.get("strict").is_none(), "{tool}");
    }
    let supported = json!({"type": "object", "properties": {
        "code": {"type": "string", "minLength": 1, "maxLength": 1000, "pattern": "^[a-z]+$"},
        "url": {"type": "string", "format": "uri"},
        "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1}}});
    assert_eq!(first_tool(strict_tool(supported)).await["strict"], true);
}

#[tokio::test]
async fn copilot_models_use_bearer_auth_and_dynamic_headers() {
    let mut copilot = test_model(API, "github-copilot", UNREACHABLE);
    copilot.headers.insert("X-Initiator".into(), "model".into());
    let image = serde_json::from_value(json!({"role": "user", "timestamp": 1,
        "content": [{"type": "image", "data": "AA==", "mimeType": "image/png"}]}))
    .unwrap();
    let context = context(vec![Message::user("hi"), Message::User(image)]);
    let request = sent(copilot, &context, StreamOptions { api_key: Some("tid_copilot".into()), ..options() }).await;
    assert_eq!(request.headers["authorization"], "Bearer tid_copilot");
    assert!(!request.headers.contains_key("x-api-key"));
    assert_eq!(request.headers["x-initiator"], "user");
    assert_eq!(request.headers["openai-intent"], "conversation-edits");
    assert_eq!(request.headers["copilot-vision-request"], "true");
}

fn tool_result(id: &str, content: Vec<ContentBlock>, is_error: bool) -> Message {
    Message::ToolResult(crate::types::ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "shot".into(),
        content,
        is_error,
        ..Default::default()
    })
}

#[tokio::test]
async fn error_tool_result_images_follow_all_tool_results() {
    let image = || ContentBlock::Image { data: "AA==".into(), mime_type: "image/png".into() };
    let call =
        |id: &str| ContentBlock::ToolCall(ToolCall { id: id.into(), name: "shot".into(), ..ToolCall::default() });
    let assistant = Message::Assistant(AssistantMessage {
        content: vec![call("t1"), call("t2")],
        api: API.into(),
        provider: "anthropic".into(),
        model: "test-model".into(),
        stop_reason: StopReason::ToolUse,
        ..Default::default()
    });
    let messages = vec![
        Message::user("go"),
        assistant,
        tool_result("t1", vec![ContentBlock::text("failed"), image(), image()], true),
        tool_result("t2", vec![image()], false),
    ];
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..options() };
    let body = capture_payload(&AnthropicMessages, &model(json!({})), &context(messages), options).await;
    let image_param = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
    assert_eq!(
        body["messages"][2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "failed\n(image attached after the tool results)"},
            {"type": "tool_result", "tool_use_id": "t2", "is_error": false,
                "content": [{"type": "text", "text": "(see attached image)"}, image_param]},
            {"type": "text", "text": "Images from the error result of tool call t1 (shot):"},
            image_param,
            image_param,
        ])
    );
}

#[tokio::test]
async fn thinking_from_another_model_is_dropped() {
    let assistant = |model: &str| {
        Message::Assistant(AssistantMessage {
            content: vec![
                ContentBlock::Thinking { thinking: "secret plan".into(), thinking_signature: None, redacted: false },
                ContentBlock::text("answer"),
            ],
            api: API.into(),
            provider: "anthropic".into(),
            model: model.into(),
            stop_reason: StopReason::Stop,
            ..Default::default()
        })
    };
    let options = StreamOptions { cache_retention: Some(CacheRetention::None), ..options() };
    for (source, expected) in [
        ("other-model", json!([{"type": "text", "text": "answer"}])),
        ("test-model", json!([{"type": "text", "text": "secret plan"}, {"type": "text", "text": "answer"}])),
    ] {
        let messages = vec![Message::user("q"), assistant(source), Message::user("next")];
        let body = capture_payload(&AnthropicMessages, &model(json!({})), &context(messages), options.clone()).await;
        assert_eq!(body["messages"][1]["content"], expected, "{source}");
    }
}
