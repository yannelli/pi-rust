//! Thinking formats and budgets, ported from upstream `openai-completions-thinking-token-budget`
//! and the thinking cases of `openai-completions-tool-choice`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::test_support::{API, capture, catalog, options, user_context};
use crate::providers::mock_server::test_model;
use crate::registry::{CacheRetention, StreamOptions, ThinkingBudgets};
use crate::types::{Model, ThinkingLevel};

fn local(compat: Value) -> Model {
    let mut model = test_model(API, "local-vllm", "http://127.0.0.1:9/v1");
    model.max_tokens = 16384;
    model.context_window = 262_144;
    model.compat = Some(compat);
    model
}

fn levels(entries: &[(&str, Option<&str>)]) -> Option<BTreeMap<String, Option<String>>> {
    Some(entries.iter().map(|(level, value)| (level.to_string(), value.map(str::to_string))).collect())
}

async fn body(model: &Model, reasoning: Option<ThinkingLevel>) -> Value {
    capture(model, &user_context("Hi"), StreamOptions { reasoning, ..options() }).await
}

async fn budget_body(model: &Model, reasoning: ThinkingLevel, budgets: ThinkingBudgets, max: Option<u64>) -> Value {
    let options =
        StreamOptions { reasoning: Some(reasoning), thinking_budgets: Some(budgets), max_tokens: max, ..options() };
    capture(model, &user_context("Hi"), options).await
}

const MEDIUM_4096: ThinkingBudgets = ThinkingBudgets { minimal: None, low: None, medium: Some(4096), high: None };
const HIGH_8192: ThinkingBudgets = ThinkingBudgets { minimal: None, low: None, medium: None, high: Some(8192) };

#[tokio::test]
async fn thinking_token_budget_uses_the_configured_level_budget() {
    let vllm = local(json!({"thinkingFormat": "zai", "supportsThinkingTokenBudget": true}));
    assert_eq!(budget_body(&vllm, ThinkingLevel::Medium, MEDIUM_4096, None).await["thinking_token_budget"], 4096);
    let plain = local(json!({"thinkingFormat": "zai"}));
    let body = budget_body(&plain, ThinkingLevel::Medium, MEDIUM_4096, None).await;
    for field in ["thinking_token_budget", "thinking_budget", "thinking_budget_tokens"] {
        assert!(body.get(field).is_none(), "{field}");
    }
    assert!(self::body(&vllm, None).await.get("thinking_token_budget").is_none(), "off sends no budget");
    for level in [ThinkingLevel::Xhigh, ThinkingLevel::Max] {
        assert_eq!(budget_body(&vllm, level, HIGH_8192, None).await["thinking_token_budget"], 8192);
    }
    let body = self::body(&vllm, Some(ThinkingLevel::High)).await;
    assert_eq!(body["thinking_token_budget"], 16384 - 1024, "answer room under the model cap");
    let body = budget_body(&vllm, ThinkingLevel::High, HIGH_8192, Some(4096)).await;
    assert_eq!(body["thinking_token_budget"], 4096 - 1024, "the caller cap is the ceiling");
}

#[tokio::test]
async fn thinking_token_budget_field_wins_over_the_alias() {
    for field in ["thinking_budget", "thinking_budget_tokens"] {
        let model = local(json!({"thinkingFormat": "qwen", "thinkingTokenBudgetField": field}));
        let body = budget_body(&model, ThinkingLevel::Medium, MEDIUM_4096, None).await;
        assert_eq!(body[field], 4096);
        assert!(body.get("thinking_token_budget").is_none());
    }
    let model = local(json!({"thinkingFormat": "zai", "supportsThinkingTokenBudget": true,
        "thinkingTokenBudgetField": "thinking_budget"}));
    let body = budget_body(&model, ThinkingLevel::Medium, MEDIUM_4096, None).await;
    assert_eq!(body["thinking_budget"], 4096);
    assert!(body.get("thinking_token_budget").is_none());
}

#[tokio::test]
async fn chat_template_kwargs_substitute_thinking_variables() {
    let budget = local(json!({"thinkingFormat": "chat-template", "chatTemplateKwargs": {
        "enable_thinking": {"$var": "thinking.enabled"}, "thinking_budget": {"$var": "thinking.budget"}}}));
    let on = body(&budget, Some(ThinkingLevel::High)).await;
    assert_eq!(on["chat_template_kwargs"], json!({"enable_thinking": true, "thinking_budget": 16384 - 1024}));
    assert!(on.get("thinking_token_budget").is_none());
    assert_eq!(body(&budget, None).await["chat_template_kwargs"], json!({"enable_thinking": false}));
    let boolean = local(json!({"thinkingFormat": "chat-template", "supportsReasoningEffort": false,
        "chatTemplateKwargs": {"thinking": {"$var": "thinking.enabled"}}}));
    for (reasoning, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let body = body(&boolean, reasoning).await;
        assert_eq!(body["chat_template_kwargs"], json!({"thinking": expected}));
        assert!(body.get("thinking").is_none() && body.get("reasoning_effort").is_none());
    }
    let mut effort = local(json!({"thinkingFormat": "chat-template", "supportsReasoningEffort": false,
        "chatTemplateKwargs": {"preserve_thinking": true,
            "reasoning_effort": {"$var": "thinking.effort", "omitWhenOff": true}}}));
    effort.thinking_level_map = levels(&[("xhigh", Some("max"))]);
    let body_xhigh = body(&effort, Some(ThinkingLevel::Xhigh)).await;
    assert_eq!(body_xhigh["chat_template_kwargs"], json!({"preserve_thinking": true, "reasoning_effort": "max"}));
    assert!(body_xhigh.get("reasoning_effort").is_none());
    assert_eq!(body(&effort, None).await["chat_template_kwargs"], json!({"preserve_thinking": true}));
}

#[tokio::test]
async fn qwen_formats_toggle_thinking() {
    let template = local(json!({"thinkingFormat": "qwen-chat-template", "supportsReasoningEffort": false}));
    for (reasoning, expected) in [(Some(ThinkingLevel::High), true), (None, false)] {
        let body = body(&template, reasoning).await;
        assert_eq!(body["chat_template_kwargs"], json!({"enable_thinking": expected, "preserve_thinking": true}));
        assert!(body.get("reasoning_effort").is_none());
    }
    let qwen = local(json!({"thinkingFormat": "qwen"}));
    let on = body(&qwen, Some(ThinkingLevel::Low)).await;
    assert_eq!((&on["enable_thinking"], &on["reasoning_effort"]), (&json!(true), &json!("low")));
    assert_eq!(body(&qwen, None).await["enable_thinking"], false);
}

#[tokio::test]
async fn zai_maps_levels_and_disables_thinking_when_off() {
    let model = catalog("zai", "glm-5.2");
    let cases = [
        (ThinkingLevel::Low, "high"),
        (ThinkingLevel::Medium, "high"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Max, "max"),
    ];
    for (level, effort) in cases {
        let body = body(&model, Some(level)).await;
        assert_eq!(body["thinking"], json!({"type": "enabled", "clear_thinking": false}));
        assert_eq!(body["reasoning_effort"], effort, "{level:?}");
    }
    let off = body(&model, None).await;
    assert_eq!(off["thinking"], json!({"type": "disabled"}));
    assert!(off.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn groq_reasoning_effort_uses_the_level_map() {
    let qwen = body(&catalog("groq", "qwen/qwen3.6-27b"), Some(ThinkingLevel::Medium)).await;
    assert_eq!(qwen["reasoning_effort"], "default");
    let gpt = body(&catalog("groq", "openai/gpt-oss-20b"), Some(ThinkingLevel::Medium)).await;
    assert_eq!(gpt["reasoning_effort"], "medium");
}

#[tokio::test]
async fn openai_format_sends_the_mapped_off_value() {
    let mut model = local(json!({}));
    model.thinking_level_map = levels(&[("off", Some("none"))]);
    assert_eq!(body(&model, None).await["reasoning_effort"], "none");
    assert_eq!(body(&model, Some(ThinkingLevel::Low)).await["reasoning_effort"], "low");
    model.thinking_level_map = None;
    assert!(body(&model, None).await.get("reasoning_effort").is_none());
}

#[tokio::test]
async fn deepseek_format_respects_unsupported_off() {
    let mimo = body(&catalog("xiaomi", "mimo-v2.6-pro"), Some(ThinkingLevel::High)).await;
    assert_eq!(mimo["thinking"], json!({"type": "enabled"}));
    assert!(mimo.get("reasoning_effort").is_none(), "MiMo only toggles thinking");
    let kimi = catalog("opencode", "kimi-k2.6");
    assert_eq!(body(&kimi, None).await["thinking"], json!({"type": "disabled"}));
    let on = body(&kimi, Some(ThinkingLevel::High)).await;
    assert_eq!(on["thinking"], json!({"type": "enabled"}));
    assert!(on.get("reasoning_effort").is_none());
    for provider in ["moonshotai", "moonshotai-cn"] {
        let off = body(&catalog(provider, "kimi-k2.7-code"), None).await;
        assert!(off.get("thinking").is_none() && off.get("reasoning_effort").is_none(), "{provider}");
    }
    assert_eq!(body(&catalog("moonshotai-cn", "kimi-k2.6"), None).await["thinking"], json!({"type": "disabled"}));
}

#[tokio::test]
async fn openrouter_and_string_thinking_formats() {
    let r1 = catalog("openrouter", "deepseek/deepseek-r1-0528");
    let on = body(&r1, Some(ThinkingLevel::High)).await;
    assert_eq!(on["reasoning"], json!({"effort": "high"}));
    assert!(on.get("reasoning_effort").is_none());
    assert!(body(&r1, None).await.get("reasoning").is_none(), "off: null omits reasoning");
    assert_eq!(body(&catalog("openrouter", "auto"), None).await["reasoning"], json!({"effort": "none"}));
    let mut string = local(json!({"thinkingFormat": "string-thinking"}));
    assert_eq!(body(&string, Some(ThinkingLevel::Medium)).await["thinking"], "medium");
    assert_eq!(body(&string, None).await["thinking"], "none");
    string.thinking_level_map = levels(&[("off", None)]);
    assert!(body(&string, None).await.get("thinking").is_none());
}

#[tokio::test]
async fn together_and_baseten_formats() {
    let together = local(json!({"thinkingFormat": "together", "supportsReasoningEffort": true}));
    let on = body(&together, Some(ThinkingLevel::High)).await;
    assert_eq!((&on["reasoning"], &on["reasoning_effort"]), (&json!({"enabled": true}), &json!("high")));
    assert_eq!(body(&together, None).await["reasoning"], json!({"enabled": false}));
    let baseten = catalog("baseten", "zai-org/GLM-5.2");
    let on = body(&baseten, Some(ThinkingLevel::High)).await;
    assert_eq!(
        (&on["chat_template_args"], &on["reasoning_effort"]),
        (&json!({"enable_thinking": true}), &json!("high"))
    );
    let off = body(&baseten, None).await;
    assert_eq!(
        (&off["chat_template_args"], &off["reasoning_effort"]),
        (&json!({"enable_thinking": false}), &json!("none"))
    );
}

#[tokio::test]
async fn ant_ling_uses_reasoning_objects_for_mapped_levels() {
    let ring = catalog("ant-ling", "Ring-2.6-1T");
    let options = StreamOptions {
        max_tokens: Some(123),
        reasoning: Some(ThinkingLevel::High),
        cache_retention: Some(CacheRetention::Long),
        session_id: Some("ant-ling-session".into()),
        ..options()
    };
    let context = crate::types::Context { system_prompt: Some("Follow instructions.".into()), ..user_context("Hi") };
    let body = capture(&ring, &context, options).await;
    assert_eq!((&body["max_tokens"], &body["messages"][0]["role"]), (&json!(123), &json!("system")));
    assert_eq!(body["reasoning"], json!({"effort": "high"}));
    for field in ["max_completion_tokens", "reasoning_effort", "store", "prompt_cache_key", "prompt_cache_retention"] {
        assert!(body.get(field).is_none(), "{field}");
    }
    let ling = self::body(&catalog("ant-ling", "Ling-2.6-flash"), Some(ThinkingLevel::High)).await;
    assert!(ling.get("reasoning").is_none());
}
