//! Request fields, ported from upstream `openai-completions-tool-choice`, `-empty-tools`,
//! `-vllm-priority`, and `sampling-options` tests.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::test_support::{
    assistant, capture, catalog, options, plain_catalog, tool, tool_call, tool_result, user_context,
};
use crate::providers::mock_server::test_model;
use crate::registry::StreamOptions;
use crate::types::{ContentBlock, Context, Message, Model, StopReason, ThinkingLevel};

const UNREACHABLE: &str = "http://127.0.0.1:9/v1";

fn gpt_4o_mini() -> Model {
    plain_catalog("openai", "gpt-4o-mini")
}

fn with_tools(text: &str, names: &[&str]) -> Context {
    Context { tools: names.iter().map(|name| tool(name)).collect(), ..user_context(text) }
}

fn custom_model(sampling: Option<Value>) -> Model {
    let mut model = test_model(super::test_support::API, "custom-provider", UNREACHABLE);
    model.reasoning = false;
    model.max_tokens = 16384;
    model.context_window = 128_000;
    model.sampling_params = sampling.and_then(|value| value.as_object().cloned());
    model
}

#[tokio::test]
async fn tool_choice_is_forwarded_with_and_without_tools() {
    let options = StreamOptions { tool_choice: Some("required".into()), ..options() };
    let body = capture(&gpt_4o_mini(), &with_tools("Call ping", &["ping"]), options).await;
    assert_eq!(body["tool_choice"], "required");
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
    let options = StreamOptions { tool_choice: Some("none".into()), ..self::options() };
    let body = capture(&gpt_4o_mini(), &user_context("Summarize"), options).await;
    assert_eq!(body["tool_choice"], "none");
    assert!(body.get("tools").is_none());
}

#[tokio::test]
async fn strict_is_sent_only_when_the_server_supports_it() {
    let context = with_tools("Call ping", &["ping"]);
    let mut model = gpt_4o_mini();
    model.compat = Some(json!({"supportsStrictMode": false}));
    let body = capture(&model, &context, options()).await;
    assert!(body["tools"][0]["function"].get("strict").is_none());
    let local = Model { reasoning: true, ..custom_model(None) };
    let body = capture(&local, &context, options()).await;
    assert!(body["tools"][0]["function"].get("strict").is_none(), "unknown endpoints default to non-strict");
    let body = capture(&catalog("groq", "openai/gpt-oss-20b"), &context, options()).await;
    assert_eq!(body["tools"][0]["function"]["strict"], false);
}

#[tokio::test]
async fn tools_are_omitted_without_tool_history_and_empty_with_it() {
    let model = gpt_4o_mini();
    let body = capture(&model, &Context { tools: vec![], ..user_context("hi") }, options()).await;
    assert!(body.get("tools").is_none());
    let call = ContentBlock::ToolCall(tool_call("t1", "noop", json!({})));
    let messages = vec![
        Message::user("use the tool"),
        assistant(&model, vec![call], StopReason::ToolUse),
        tool_result("t1", "noop", vec![ContentBlock::text("done")]),
    ];
    let body = capture(&model, &Context { messages, ..Context::default() }, options()).await;
    assert_eq!(body["tools"], json!([]));
}

#[tokio::test]
async fn max_tokens_default_explicit_and_context_clamped() {
    let model = gpt_4o_mini();
    let body = capture(&model, &user_context("hi"), options()).await;
    assert_eq!((body.get("max_tokens"), &body["max_completion_tokens"]), (None, &json!(model.max_tokens)));
    let body = capture(&model, &user_context("hi"), StreamOptions { max_tokens: Some(1234), ..options() }).await;
    assert_eq!(body["max_completion_tokens"], 1234);
    let small = Model { context_window: 10_000, max_tokens: 8000, ..gpt_4o_mini() };
    let long = user_context(&"x".repeat(8000));
    for max_tokens in [None, Some(7000)] {
        let body = capture(&small, &long, StreamOptions { max_tokens, ..options() }).await;
        assert_eq!(body["max_completion_tokens"], 3618);
    }
}

#[tokio::test]
async fn max_tokens_field_follows_provider_detection() {
    let deepseek = Model {
        id: "custom-deepseek-model".into(),
        provider: "custom-deepseek".into(),
        base_url: "https://API.DeepSeek.COM".into(),
        ..custom_model(None)
    };
    let models = [
        catalog("opencode-go", "kimi-k3"),
        catalog("opencode", "kimi-k2.6"),
        catalog("deepseek", "deepseek-flash"),
        catalog("zai", "glm-5.2"),
        deepseek,
    ];
    for model in models {
        let body = capture(&model, &user_context("hi"), StreamOptions { max_tokens: Some(123), ..options() }).await;
        assert_eq!(body["max_tokens"], 123, "{}", model.id);
        assert!(body.get("max_completion_tokens").is_none(), "{}", model.id);
    }
}

#[tokio::test]
async fn vllm_priority_is_a_top_level_field() {
    let mut model = gpt_4o_mini();
    model.compat = Some(json!({"vllmPriority": 10}));
    assert_eq!(capture(&model, &user_context("hi"), options()).await["priority"], 10);
    assert!(capture(&gpt_4o_mini(), &user_context("hi"), options()).await.get("priority").is_none());
}

#[tokio::test]
async fn zai_tool_stream_needs_tools() {
    let model = catalog("zai", "glm-5.2");
    let body = capture(&model, &with_tools("Call ping", &["ping"]), options()).await;
    assert_eq!(body["tool_stream"], true);
    assert!(capture(&model, &user_context("hi"), options()).await.get("tool_stream").is_none());
}

#[tokio::test]
async fn instruction_role_depends_on_developer_support() {
    let context = Context { system_prompt: Some("Follow instructions.".into()), ..Context::default() };
    let first_role = |body: Value| body["messages"][0]["role"].as_str().unwrap().to_string();
    let deepseek = capture(&catalog("openrouter", "deepseek/deepseek-v4-pro"), &context, options()).await;
    assert_eq!(first_role(deepseek), "system");
    for (provider, id) in [("openrouter", "openai/gpt-5.2-codex"), ("openrouter", "anthropic/claude-fable-5.1:batch")] {
        assert_eq!(first_role(capture(&catalog(provider, id), &context, options()).await), "developer", "{id}");
    }
    assert_eq!(first_role(capture(&plain_catalog("openai", "gpt-5.5"), &context, options()).await), "developer");
    let cloudflare = Model { provider: "cloudflare-ai-gateway".into(), reasoning: true, ..custom_model(None) };
    let body =
        capture(&cloudflare, &context, StreamOptions { reasoning: Some(ThinkingLevel::High), ..options() }).await;
    assert_eq!(first_role(body.clone()), "system");
    assert!(body.get("store").is_none() && body.get("reasoning_effort").is_none());
    assert!(body.get("max_tokens").is_some() && body.get("max_completion_tokens").is_none());
}

#[tokio::test]
async fn routing_preferences_are_forwarded() {
    let mut model = custom_model(None);
    model.compat = Some(json!({
        "openRouterRouting": {"only": ["anthropic"]},
        "vercelGatewayRouting": {"only": ["bedrock"], "order": ["bedrock", "anthropic"]},
    }));
    let body = capture(&model, &user_context("hi"), options()).await;
    assert_eq!(body["provider"], json!({"only": ["anthropic"]}));
    assert_eq!(body["providerOptions"], json!({"gateway": {"only": ["bedrock"], "order": ["bedrock", "anthropic"]}}));
}

#[tokio::test]
async fn sampling_params_merge_model_level_and_request_values_last() {
    let context = user_context("Hello");
    let request = |value: Value| StreamOptions { sampling_params: value.as_object().cloned(), ..options() };
    let body = capture(&custom_model(None), &context, request(json!({"top_p": 0.95, "top_k": 0, "min_p": 0}))).await;
    assert_eq!((&body["top_p"], &body["top_k"], &body["min_p"]), (&json!(0.95), &json!(0), &json!(0)));
    let body = capture(&custom_model(None), &context, options()).await;
    assert!(body.get("temperature").is_none() && body.get("top_p").is_none());
    let model = custom_model(Some(json!({"top_p": 0.95, "min_p": 0.05})));
    let body = capture(&model, &context, request(json!({"top_p": 0.5}))).await;
    assert_eq!((&body["top_p"], &body["min_p"]), (&json!(0.5), &json!(0.05)));
    let options = StreamOptions { temperature: Some(0.0), ..request(json!({"temperature": 1})) };
    assert_eq!(capture(&custom_model(None), &context, options).await["temperature"], 1);
}

#[tokio::test]
async fn sampling_params_follow_the_effective_thinking_level() {
    let context = user_context("Hello");
    let by_level =
        |levels: Value| -> Option<BTreeMap<String, crate::types::JsonObject>> { serde_json::from_value(levels).ok() };
    let mut model = custom_model(Some(json!({"temperature": 1, "top_p": 0.95})));
    model.reasoning = true;
    model.thinking_level_map = Some(BTreeMap::from([("low".into(), None), ("medium".into(), None)]));
    model.sampling_params_by_thinking_level = by_level(json!({"high": {"temperature": 0.8, "top_k": 64}}));
    let body = capture(&model, &context, StreamOptions { reasoning: Some(ThinkingLevel::Low), ..options() }).await;
    assert_eq!((&body["temperature"], &body["top_p"], &body["top_k"]), (&json!(0.8), &json!(0.95), &json!(64)));
    let mut off = custom_model(None);
    off.sampling_params_by_thinking_level = by_level(json!({"off": {"temperature": 0.7}}));
    assert_eq!(capture(&off, &context, options()).await["temperature"], 0.7);
    let mut low = custom_model(None);
    low.reasoning = true;
    low.sampling_params_by_thinking_level = by_level(json!({"low": {"temperature": 0.6, "top_p": 0.95}}));
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::Low),
        sampling_params: json!({"top_p": 0.5}).as_object().cloned(),
        ..options()
    };
    let body = capture(&low, &context, options).await;
    assert_eq!((&body["temperature"], &body["top_p"]), (&json!(0.6), &json!(0.5)));
}
