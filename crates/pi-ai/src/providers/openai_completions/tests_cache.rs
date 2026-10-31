//! Prompt caching, session affinity headers, and Anthropic-style cache markers, ported from
//! upstream `openai-completions-prompt-cache`, `-cache-control-format`, and `cache-retention`.

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};

use super::OpenAICompletions;
use super::test_support::{
    assistant, capture, catalog, ok_frames, options, plain_catalog, tool, tool_call, tool_result, user_context,
};
use crate::providers::mock_server::{MockResponse, MockServer, collect};
use crate::registry::{ApiProvider, CacheRetention, StreamOptions};
use crate::types::{ContentBlock, Context, Message, Model, StopReason};

fn openai() -> Model {
    plain_catalog("openai", "gpt-4o-mini")
}

fn proxy(compat: Value) -> Model {
    Model { base_url: "https://proxy.example.com/v1".into(), compat: Some(compat), ..openai() }
}

fn session(id: &str, retention: Option<CacheRetention>) -> StreamOptions {
    StreamOptions { session_id: Some(id.into()), cache_retention: retention, ..options() }
}

fn prompt() -> Context {
    Context { system_prompt: Some("sys".into()), ..user_context("hi") }
}

/// Headers the server received for one request to `model`.
async fn headers(model: &Model, options: StreamOptions) -> HashMap<String, String> {
    let mut server = MockServer::start(MockResponse::Sse(ok_frames())).await;
    let model = Model { base_url: server.base_url.clone(), ..model.clone() };
    collect(OpenAICompletions.stream(&model, &prompt(), options)).await;
    server.request().await.headers
}

#[tokio::test]
async fn direct_openai_requests_send_prompt_cache_fields() {
    let body = capture(&openai(), &prompt(), session("session-123", None)).await;
    assert_eq!(body["prompt_cache_key"], "session-123");
    assert!(body.get("prompt_cache_retention").is_none());
    let body = capture(&openai(), &prompt(), session("session-456", Some(CacheRetention::Long))).await;
    assert_eq!((&body["prompt_cache_key"], &body["prompt_cache_retention"]), (&json!("session-456"), &json!("24h")));
    let body = capture(&openai(), &prompt(), session(&"x".repeat(67), None)).await;
    assert_eq!(body["prompt_cache_key"], "x".repeat(64));
    let body = capture(&openai(), &prompt(), session("session-789", Some(CacheRetention::None))).await;
    assert!(body.get("prompt_cache_key").is_none() && body.get("prompt_cache_retention").is_none());
    let env = BTreeMap::from([("PI_CACHE_RETENTION".to_string(), "long".to_string())]);
    let body = capture(&openai(), &prompt(), StreamOptions { env, ..session("session-env", None) }).await;
    assert_eq!((&body["prompt_cache_key"], &body["prompt_cache_retention"]), (&json!("session-env"), &json!("24h")));
}

#[tokio::test]
async fn other_endpoints_send_prompt_cache_fields_only_for_long_retention() {
    let long = || session("session-completions", Some(CacheRetention::Long));
    let body = capture(&proxy(json!({})), &prompt(), long()).await;
    assert_eq!(body["prompt_cache_key"], "session-completions");
    assert_eq!(body["prompt_cache_retention"], "24h");
    let body = capture(&proxy(json!({})), &prompt(), session("short", None)).await;
    assert!(body.get("prompt_cache_key").is_none());
    let unsupported = [
        proxy(json!({"supportsLongCacheRetention": false})),
        catalog("opencode", "deepseek-v4.1-flash"),
        catalog("opencode", "kimi-k2.6"),
        catalog("opencode", "minimax-m2.7"),
    ];
    for model in unsupported {
        let body = capture(&model, &prompt(), long()).await;
        assert!(body.get("prompt_cache_key").is_none(), "{}", model.id);
        assert!(body.get("prompt_cache_retention").is_none(), "{}", model.id);
    }
}

#[tokio::test]
async fn session_affinity_headers_follow_the_configured_format() {
    let openai_format = headers(&proxy(json!({"sendSessionAffinityHeaders": true})), session("aff", None)).await;
    for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert_eq!(openai_format.get(name).map(String::as_str), Some("aff"), "{name}");
    }
    let nosession = proxy(json!({"sendSessionAffinityHeaders": true, "sessionAffinityFormat": "openai-nosession"}));
    let received = headers(&nosession, session("ns", None)).await;
    assert!(!received.contains_key("session_id") && !received.contains_key("x-session-id"));
    assert_eq!((&received["x-client-request-id"], &received["x-session-affinity"]), (&"ns".into(), &"ns".into()));
    let openrouter = proxy(json!({"sendSessionAffinityHeaders": true, "sessionAffinityFormat": "openrouter"}));
    let received = headers(&openrouter, session("or", None)).await;
    assert_eq!(received["x-session-id"], "or");
    for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert!(!received.contains_key(name), "{name}");
    }
    let body = capture(&openrouter, &prompt(), session("or", None)).await;
    assert!(body.get("prompt_cache_key").is_none() && body.get("session_id").is_none());
}

#[tokio::test]
async fn catalog_models_send_their_session_affinity_headers() {
    for id in ["accounts/fireworks/models/glm-5p3", "accounts/fireworks/routers/glm-5p3-fast"] {
        let received = headers(&catalog("fireworks", id), session("fireworks-session", None)).await;
        assert_eq!(received["x-session-affinity"], "fireworks-session", "{id}");
    }
    let received = headers(&catalog("baseten", "zai-org/GLM-5.2"), session("baseten", None)).await;
    assert_eq!(
        (&received["x-session-affinity"], &received["x-client-request-id"]),
        (&"baseten".into(), &"baseten".into())
    );
    let received = headers(&catalog("openrouter", "auto"), session("router", None)).await;
    assert_eq!(received["x-session-id"], "router");
    assert!(!received.contains_key("session_id") && !received.contains_key("x-session-affinity"));
    let disabled = Model {
        provider: "openrouter".into(),
        base_url: "https://openrouter.ai/api/v1".into(),
        compat: Some(json!({"sendSessionAffinityHeaders": false})),
        ..openai()
    };
    assert!(!headers(&disabled, session("router", None)).await.contains_key("x-session-id"));
}

#[tokio::test]
async fn session_headers_skip_disabled_caching_and_yield_to_request_headers() {
    let model = proxy(json!({"sendSessionAffinityHeaders": true}));
    let received = headers(&model, session("aff", Some(CacheRetention::None))).await;
    for name in ["session_id", "x-client-request-id", "x-session-affinity"] {
        assert!(!received.contains_key(name), "{name}");
    }
    let mut options = session("aff", None);
    options.headers.insert("session_id".into(), "override-session".into());
    options.headers.insert("x-client-request-id".into(), "override-request".into());
    let mut with_model_header = model.clone();
    with_model_header.headers.insert("X-Session-Affinity".into(), "model-value".into());
    let received = headers(&with_model_header, options).await;
    assert_eq!(received["session_id"], "override-session");
    assert_eq!(received["x-client-request-id"], "override-request");
    assert_eq!(received["x-session-affinity"], "aff", "generated headers replace model headers");
}

fn cache_context(messages: Vec<Message>) -> Context {
    Context { system_prompt: Some("System prompt".into()), messages, tools: vec![tool("read")] }
}

fn custom_anthropic_format() -> Model {
    Model {
        id: "custom-qwen".into(),
        provider: "openrouter".into(),
        base_url: "https://example.com/v1".into(),
        compat: Some(json!({"cacheControlFormat": "anthropic"})),
        ..openai()
    }
}

fn assert_markers(body: &Value, marker: &Value) {
    let instruction = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| matches!(message["role"].as_str(), Some("system" | "developer")));
    assert_eq!(instruction.unwrap()["content"][0]["cache_control"], *marker);
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["tools"][0]["cache_control"], *marker);
    let last = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["cache_control"], *marker);
}

#[tokio::test]
async fn anthropic_cache_markers_cover_prompt_tools_and_last_message() {
    let context = cache_context(vec![Message::user("Hello")]);
    let ephemeral = json!({"type": "ephemeral"});
    assert_markers(&capture(&custom_anthropic_format(), &context, options()).await, &ephemeral);
    let batch = catalog("openrouter", "anthropic/claude-fable-5.1:batch");
    assert_markers(&capture(&batch, &context, options()).await, &ephemeral);
    let long = StreamOptions { cache_retention: Some(CacheRetention::Long), ..options() };
    assert_markers(&capture(&batch, &context, long).await, &json!({"type": "ephemeral", "ttl": "1h"}));
    let none = StreamOptions { cache_retention: Some(CacheRetention::None), ..options() };
    let body = capture(&custom_anthropic_format(), &context, none).await;
    assert!(body["messages"][0]["content"].is_string());
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(body["messages"].as_array().unwrap().last().unwrap()["content"].is_string());
    for id in ["fable", "haiku", "opus", "sonnet"] {
        let model = catalog("openrouter", &format!("~anthropic/claude-{id}-latest"));
        assert_eq!(model.api, "anthropic-messages", "{id}");
        assert_eq!(model.compat.as_ref().unwrap()["forceAdaptiveThinking"], true, "{id}");
    }
}

#[tokio::test]
async fn the_conversation_marker_moves_to_a_trailing_tool_result() {
    let model = catalog("openrouter", "anthropic/claude-fable-5.1:batch");
    let call = ContentBlock::ToolCall(tool_call("call_1", "read", json!({"path": "README.md"})));
    let messages = vec![
        Message::user("Read the file"),
        assistant(&model, vec![call], StopReason::ToolUse),
        tool_result("call_1", "read", vec![ContentBlock::text("file contents")]),
    ];
    let body = capture(&model, &cache_context(messages), options()).await;
    let user = body["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user").unwrap();
    assert_eq!(user["content"], "Read the file");
    let last = body["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(last["role"], "tool");
    assert_eq!(last["content"][0]["cache_control"], json!({"type": "ephemeral"}));
}
