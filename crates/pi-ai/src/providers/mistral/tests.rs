use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::MistralConversations;
use super::request::{build_payload, derive_tool_call_id};
use crate::providers::mock_http::{Body, MockHttp, Reply};
use crate::providers::mock_server::{collect, test_model};
use crate::registry::{ApiProvider, CacheRetention, ProviderResponse, StreamOptions};
use crate::types::{
    AssistantMessage, ContentBlock, Context, Message, Model, StopReason, ThinkingLevel, Tool, ToolCall,
    ToolResultMessage, UserContent, UserMessage,
};

const API: &str = "mistral-conversations";

fn sse(events: &[Value]) -> Reply {
    let mut frames: Vec<String> = events.iter().map(|event| format!("data: {event}\r\n\r\n")).collect();
    frames.push("data: [DONE]\r\n\r\n".into());
    Reply::sse(frames)
}

fn terminal(reason: &str) -> Value {
    json!({"id": "mistral-response-id", "choices": [{"index": 0, "finish_reason": reason, "delta": {}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}})
}

fn delta(delta: Value, finish: Option<&str>) -> Value {
    json!({"id": "response-1", "choices": [{"index": 0, "finish_reason": finish, "delta": delta}]})
}

fn model(base_url: &str) -> Model {
    Model { id: "mistral-large-latest".into(), reasoning: false, ..test_model(API, "mistral", base_url) }
}

fn hello() -> Context {
    Context { messages: vec![Message::user("hello")], ..Context::default() }
}

fn keyed() -> StreamOptions {
    StreamOptions { api_key: Some("secret".into()), ..StreamOptions::default() }
}

fn image(data: &str) -> ContentBlock {
    ContentBlock::Image { data: data.into(), mime_type: "image/png".into() }
}

#[tokio::test]
async fn serializes_sdk_payloads_to_the_wire_format() {
    let server = MockHttp::start(vec![sse(&[terminal("stop")]).header("x-request-id", "request-1")]).await;
    let user = Message::User(UserMessage {
        content: UserContent::Blocks(vec![ContentBlock::text("describe"), image("aGVsbG8=")]),
        timestamp: 1,
    });
    let tool = Tool::new(
        "lookup",
        "Look something up",
        json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    );
    let context = Context { system_prompt: Some("Be precise".into()), messages: vec![user], tools: vec![tool] };
    let payloads = Arc::new(Mutex::new(Vec::<Value>::new()));
    let responses = Arc::new(Mutex::new(Vec::<ProviderResponse>::new()));
    let (payload_hook, response_hook) = (payloads.clone(), responses.clone());
    let options = StreamOptions {
        headers: BTreeMap::from([("x-custom".to_string(), "value".to_string())]),
        max_tokens: Some(123),
        tool_choice: Some("lookup".into()),
        session_id: Some("session-1".into()),
        on_payload: Some(crate::registry::payload_hook(move |payload: &Value, _: &Model| {
            payload_hook.lock().unwrap().push(payload.clone());
            let mut next = payload.clone();
            let extra = json!({"topP": 0.9, "randomSeed": 42, "presencePenalty": 0.1, "frequencyPenalty": 0.2,
                "parallelToolCalls": true, "safePrompt": true, "promptMode": "reasoning", "reasoningEffort": "high",
                "responseFormat": {"type": "json_schema", "jsonSchema": {"name": "result",
                    "schemaDefinition": {"type": "object", "properties": {"maxTokens": {"type": "number"}}}}}});
            next.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            Some(next)
        })),
        on_response: Some(Arc::new(move |response: &ProviderResponse, _: &Model| {
            response_hook.lock().unwrap().push(response.clone())
        })),
        ..keyed()
    };
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &context, options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = server.request(0);
    assert_eq!(request.path, "/v1/chat/completions");
    assert_eq!(request.headers["authorization"], "Bearer secret");
    assert_eq!(request.headers["accept"], "text/event-stream");
    assert_eq!(request.headers["x-affinity"], "session-1");
    assert_eq!(request.headers["x-custom"], "value");
    let payload = payloads.lock().unwrap()[0].clone();
    assert_eq!((payload["maxTokens"].clone(), payload["promptCacheKey"].clone()), (json!(123), json!("session-1")));
    let response = responses.lock().unwrap()[0].clone();
    assert_eq!((response.status, response.headers["x-request-id"].as_str()), (200, "request-1"));
    let wire = request.json();
    for (key, value) in [
        ("max_tokens", json!(123)),
        ("prompt_mode", json!("reasoning")),
        ("reasoning_effort", json!("high")),
        ("prompt_cache_key", json!("session-1")),
        ("top_p", json!(0.9)),
        ("random_seed", json!(42)),
        ("presence_penalty", json!(0.1)),
        ("frequency_penalty", json!(0.2)),
        ("parallel_tool_calls", json!(true)),
        ("safe_prompt", json!(true)),
        ("tool_choice", json!({"type": "function", "function": {"name": "lookup"}})),
    ] {
        assert_eq!(wire[key], value, "{key}");
    }
    assert_eq!(
        wire["response_format"],
        json!({"type": "json_schema", "json_schema": {"name": "result", "schema": {"type": "object", "properties": {"maxTokens": {"type": "number"}}}}})
    );
    assert!(
        wire.get("maxTokens").is_none() && wire.get("promptMode").is_none() && wire.get("promptCacheKey").is_none()
    );
    assert_eq!(
        wire["messages"],
        json!([{"role": "system", "content": "Be precise"}, {"role": "user", "content": [
            {"type": "text", "text": "describe"}, {"type": "image_url", "image_url": "data:image/png;base64,aGVsbG8="}]}])
    );
    assert_eq!(wire["tools"][0]["function"]["strict"], false);
}

#[tokio::test]
async fn replays_thinking_tool_calls_and_tool_results() {
    let server = MockHttp::start(vec![sse(&[terminal("stop")])]).await;
    let target = model(&server.base_url);
    let call = ToolCall {
        id: "abc123456".into(),
        name: "lookup".into(),
        arguments: json!({"query": "pi"}).as_object().unwrap().clone(),
        ..ToolCall::default()
    };
    let assistant = Message::Assistant(AssistantMessage {
        content: vec![
            ContentBlock::Thinking { thinking: "reason".into(), thinking_signature: None, redacted: false },
            ContentBlock::text("answer"),
            ContentBlock::ToolCall(call),
        ],
        api: API.into(),
        provider: "mistral".into(),
        model: target.id.clone(),
        stop_reason: StopReason::ToolUse,
        ..AssistantMessage::default()
    });
    let result = Message::ToolResult(ToolResultMessage {
        tool_call_id: "abc123456".into(),
        tool_name: "lookup".into(),
        content: vec![ContentBlock::text("found"), image("aGVsbG8=")],
        ..ToolResultMessage::default()
    });
    let context = Context { messages: vec![assistant, result], ..Context::default() };
    let (_, message) = collect(MistralConversations.stream(&target, &context, keyed())).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        server.request(0).json()["messages"],
        json!([
            {"role": "assistant", "prefix": false,
             "content": [{"type": "thinking", "thinking": [{"type": "text", "text": "reason"}]}, {"type": "text", "text": "answer"}],
             "tool_calls": [{"id": "abc123456", "type": "function", "function": {"name": "lookup", "arguments": "{\"query\":\"pi\"}"}, "index": 0}]},
            {"role": "tool", "tool_call_id": "abc123456", "name": "lookup",
             "content": [{"type": "text", "text": "found"}, {"type": "image_url", "image_url": "data:image/png;base64,aGVsbG8="}]}
        ])
    );
}

#[tokio::test]
async fn parses_thinking_text_fragmented_tool_calls_and_cached_usage() {
    let thinking = json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "reason"}]}]});
    let first = json!({"tool_calls": [{"id": "abc123456", "index": 0, "function": {"name": "lookup", "arguments": "{\"query\":"}}]});
    let mut last = delta(
        json!({"tool_calls": [{"index": 0, "function": {"name": "", "arguments": "\"pi\"}"}}]}),
        Some("tool_calls"),
    );
    last["usage"] = json!({"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14, "prompt_tokens_details": {"cached_tokens": 3}});
    let events = [
        delta(thinking, None),
        delta(json!({"content": [{"type": "text", "text": "answer"}]}), None),
        delta(first, None),
        last,
    ];
    let server = MockHttp::start(vec![sse(&events)]).await;
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), keyed())).await;
    assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (StopReason::ToolUse, Some("tool_calls")));
    assert_eq!(message.response_id.as_deref(), Some("response-1"));
    assert!(matches!(&message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "reason"));
    assert_eq!(message.content[1], ContentBlock::text("answer"));
    let call = message.tool_calls().next().unwrap();
    assert_eq!((call.id.as_str(), call.name.as_str(), &call.arguments["query"]), ("abc123456", "lookup", &json!("pi")));
    let usage = message.usage;
    assert_eq!((usage.input, usage.output, usage.cache_read, usage.cache_write, usage.total_tokens), (7, 4, 3, 0, 14));
    assert!((usage.cost.total - (7.0 + 4.0 * 2.0 + 3.0 * 0.5) / 1e6).abs() < 1e-12);
}

#[tokio::test]
async fn empty_content_deltas_do_not_split_blocks() {
    let thinking = |text: &str| json!({"type": "thinking", "thinking": [{"type": "text", "text": text}]});
    let call = |arguments: &str, first: bool| {
        let mut call =
            json!({"index": 0, "function": {"name": if first { "read" } else { "" }, "arguments": arguments}});
        if first {
            call["id"] = json!("abc123456");
        }
        call
    };
    let deltas = [
        json!({"content": ""}),
        json!({"content": [thinking("first part,")]}),
        json!({"content": ""}),
        json!({"content": [{"type": "text", "text": ""}]}),
        json!({"content": [thinking(" second part."), {"type": "text", "text": "Reading."}]}),
        json!({"content": "", "tool_calls": [call("", true)]}),
        json!({"content": "", "tool_calls": [call("{\"path\":", false)]}),
        json!({"content": "", "tool_calls": [call("\"a.txt\"}", false)]}),
        json!({"content": ""}),
    ];
    let count = deltas.len();
    let events: Vec<Value> =
        deltas.into_iter().enumerate().map(|(i, d)| delta(d, (i == count - 1).then_some("tool_calls"))).collect();
    let server = MockHttp::start(vec![sse(&events)]).await;
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), keyed())).await;
    assert_eq!(message.content.len(), 3);
    assert!(
        matches!(&message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "first part, second part.")
    );
    assert_eq!(message.content[1], ContentBlock::text("Reading."));
    assert_eq!(message.tool_calls().next().unwrap().arguments["path"], "a.txt");
}

#[tokio::test]
async fn forwards_stream_events_and_decodes_unicode() {
    let events = [
        json!({"id": "response-1", "provider_metadata": {"request": "test"}, "choices": [{"index": 0, "finish_reason": null, "delta": {"content": "héllo 🌍"}}]}),
        terminal("stop"),
    ];
    let server = MockHttp::start(vec![sse(&events)]).await;
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let hook = seen.clone();
    let options = StreamOptions {
        on_provider_stream_event: Some(Arc::new(move |event: &Value, _: &Model| {
            hook.lock().unwrap().push(event.clone())
        })),
        ..keyed()
    };
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), options)).await;
    assert_eq!((message.stop_reason, message.text().as_str()), (StopReason::Stop, "héllo 🌍"));
    assert_eq!(*seen.lock().unwrap(), events);
}

#[tokio::test]
async fn header_overrides_and_suppression_are_case_insensitive() {
    let server = MockHttp::start(vec![sse(&[terminal("stop")])]).await;
    let mut target = model(&server.base_url);
    target.headers = BTreeMap::from([
        ("Authorization".into(), "Bearer model-key".into()),
        ("X-Affinity".into(), "model-affinity".into()),
    ]);
    let options = StreamOptions {
        session_id: Some("automatic-affinity".into()),
        headers: BTreeMap::from([("User-Agent".to_string(), "custom-agent".to_string())]),
        suppressed_headers: vec!["authorization".into(), "x-affinity".into()],
        ..keyed()
    };
    collect(MistralConversations.stream(&target, &hello(), options)).await;
    let headers = server.request(0).headers;
    assert!(!headers.contains_key("authorization") && !headers.contains_key("x-affinity"));
    assert_eq!(headers["user-agent"], "custom-agent");
}

#[tokio::test]
async fn aborts_times_out_headers_and_allows_long_streams() {
    let server =
        MockHttp::start(vec![Reply { body: Body::ChunksThenHang(Vec::new()), ..Reply::sse(Vec::new()) }]).await;
    let options = keyed();
    let cancel = options.cancel.clone();
    let stream = MistralConversations.stream(&model(&server.base_url), &hello(), options);
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let (_, message) = collect(stream).await;
    assert_eq!(message.stop_reason, StopReason::Aborted);

    let server = MockHttp::start(vec![Reply { body: Body::Silent, ..Reply::sse(Vec::new()) }]).await;
    let options = StreamOptions { timeout_ms: Some(20), ..keyed() };
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), options)).await;
    assert_eq!(message.error_message.as_deref(), Some("Mistral response headers timed out after 20ms"));

    let thinking = delta(json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "x"}]}]}), None);
    let mut frames = vec![format!("data: {thinking}\n\n"); 5];
    frames.push(format!("data: {}\n\ndata: [DONE]\n\n", terminal("stop")));
    let slow = Reply { body: Body::Slow(frames, Duration::from_millis(15)), ..Reply::sse(Vec::new()) };
    let server = MockHttp::start(vec![slow]).await;
    let options = StreamOptions { timeout_ms: Some(20), ..keyed() };
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert!(matches!(&message.content[0], ContentBlock::Thinking { thinking, .. } if thinking == "xxxxx"));
}

#[tokio::test]
async fn errors_keep_status_body_and_raw_stop_reasons() {
    let server =
        MockHttp::start(vec![Reply::text(403, "application/json", r#"{"message":"blocked by gateway"}"#)]).await;
    let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), keyed())).await;
    assert_eq!(message.error_message.as_deref(), Some(r#"Mistral API error (403): {"message":"blocked by gateway"}"#));
    for (reason, stop, error) in [
        ("stop", StopReason::Stop, None),
        ("error", StopReason::Error, Some("Provider stopped with: error (server error)")),
        ("unmapped_error", StopReason::Error, Some("Provider stopped with: unmapped_error")),
    ] {
        let server = MockHttp::start(vec![sse(&[terminal(reason)])]).await;
        let (_, message) = collect(MistralConversations.stream(&model(&server.base_url), &hello(), keyed())).await;
        assert_eq!((message.stop_reason, message.raw_stop_reason.as_deref()), (stop, Some(reason)));
        assert_eq!(message.error_message.as_deref(), error);
        assert_eq!(crate::retry::is_retryable_assistant_error(&message), reason == "error", "{reason}");
    }
    let missing = StreamOptions { api_key: None, ..StreamOptions::default() };
    let (_, message) = collect(MistralConversations.stream(&model("http://127.0.0.1:9"), &hello(), missing)).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: mistral"));
}

fn reasoning_model(id: &str, reasoning: bool, map: &[(&str, Option<&str>)]) -> Model {
    let map = (!map.is_empty()).then(|| map.iter().map(|(k, v)| (k.to_string(), v.map(str::to_string))).collect());
    Model { id: id.into(), reasoning, thinking_level_map: map, ..test_model(API, "mistral", "http://127.0.0.1:9") }
}

fn controls(model: &Model, reasoning: Option<ThinkingLevel>) -> (Value, Value) {
    let options = StreamOptions { reasoning, ..StreamOptions::default() };
    let payload = build_payload(model, &hello(), &options).unwrap();
    (payload["promptMode"].clone(), payload["reasoningEffort"].clone())
}

#[test]
fn reasoning_mode_selection_matches_upstream() {
    let magistral = reasoning_model("magistral-medium-latest", true, &[]);
    assert_eq!(controls(&magistral, Some(ThinkingLevel::Medium)), (json!("reasoning"), Value::Null));
    assert_eq!(controls(&magistral, None), (Value::Null, Value::Null));
    let none_high = [
        ("off", Some("none")),
        ("minimal", None),
        ("low", None),
        ("medium", None),
        ("high", Some("high")),
        ("xhigh", None),
        ("max", None),
    ];
    let medium = reasoning_model("mistral-medium-latest", true, &none_high);
    assert_eq!(controls(&medium, Some(ThinkingLevel::High)), (Value::Null, json!("high")));
    assert_eq!(controls(&medium, Some(ThinkingLevel::Low)).1, json!("high"));
    assert_eq!(controls(&medium, None), (Value::Null, json!("none")));
    let mut glm = none_high;
    glm[0] = ("off", None);
    glm[2] = ("low", Some("low"));
    glm[6] = ("max", Some("max"));
    let glm = reasoning_model("zai-glm-5-3", true, &glm);
    for (level, expected) in [
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Max, "max"),
        (ThinkingLevel::Medium, "high"),
    ] {
        assert_eq!(controls(&glm, Some(level)).1, json!(expected));
    }
    assert_eq!(
        controls(&reasoning_model("mistral-medium-2505", false, &[]), Some(ThinkingLevel::Medium)),
        (Value::Null, Value::Null)
    );
    let cached = |retention| {
        let options = StreamOptions {
            session_id: Some("session-123".into()),
            cache_retention: retention,
            ..StreamOptions::default()
        };
        build_payload(&magistral, &hello(), &options).unwrap()["promptCacheKey"].clone()
    };
    assert_eq!((cached(None), cached(Some(CacheRetention::None))), (json!("session-123"), Value::Null));
}

#[test]
fn strict_tools_and_foreign_tool_call_ids() {
    let mut tool = Tool::new(
        "inspect_schema",
        "Inspect",
        json!({"type": "object", "properties": {"nested": {"type": "object", "properties": {"value": {"type": "string"}}, "required": ["value"]}}, "required": ["nested"]}),
    );
    tool.constrained_sampling = Some(json!({"type": "json_schema", "strict": "require"}));
    let context = Context { tools: vec![tool], ..hello() };
    let payload = build_payload(&model("http://127.0.0.1:9"), &context, &StreamOptions::default()).unwrap();
    assert_eq!(payload["tools"][0]["function"]["strict"], true);
    assert_eq!(payload["tools"][0]["function"]["parameters"]["additionalProperties"], false);

    assert_eq!(derive_tool_call_id("abc123456", 0), "abc123456");
    let foreign = derive_tool_call_id("call_|fc_123", 0);
    assert_eq!(foreign.len(), 9);
    assert!(foreign.chars().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(derive_tool_call_id("call_|fc_123", 1), foreign);
}
