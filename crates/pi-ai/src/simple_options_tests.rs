use serde_json::json;

use super::*;
use crate::estimate::ContextUsageEstimate;
use crate::transcript::normalize_context;
use crate::types::{AssistantMessage, Context, StopReason, Usage, UserContent, UserMessage};

fn model() -> Model {
    Model {
        id: "test-model".into(),
        api: "openai-responses".into(),
        provider: "openai".into(),
        context_window: 10_000,
        max_tokens: 8_000,
        ..Default::default()
    }
}

fn user(text: &str, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(text.into()), timestamp })
}

fn assistant(timestamp: i64, total_tokens: u64) -> Message {
    Message::Assistant(AssistantMessage {
        content: vec![crate::types::ContentBlock::text("kept")],
        usage: Usage { input: total_tokens, total_tokens, ..Usage::default() },
        stop_reason: StopReason::Stop,
        timestamp,
        ..Default::default()
    })
}

#[test]
fn output_cap_reserves_room_for_estimated_context() {
    let messages = normalize_context(&Context {
        messages: vec![assistant(100, 2_000), user(&"x".repeat(3_500), 200)],
        ..Context::default()
    });
    assert_eq!(
        crate::estimate::estimate_context_tokens(&messages),
        ContextUsageEstimate { tokens: 3_000, usage_tokens: 2_000, trailing_tokens: 1_000, last_usage_index: Some(0) }
    );
    assert_eq!(clamp_max_tokens_to_context(&model(), &messages, 8_000), 2_904);

    let messages = normalize_context(&Context {
        system_prompt: Some("system".into()),
        messages: vec![user("summary", 200), assistant(100, 9_500), user(&"x".repeat(4_000), 300)],
        ..Context::default()
    });
    assert_eq!(crate::estimate::estimate_context_tokens(&messages).tokens, 1_149);
    assert_eq!(clamp_max_tokens_to_context(&model(), &messages, 8_000), 4_755);

    let messages = vec![
        user("summary", 200),
        assistant(100, 9_500),
        user("new prompt", 300),
        assistant(400, 2_000),
        user("tail", 500),
    ];
    let estimate = crate::estimate::estimate_context_tokens(&messages);
    assert_eq!((estimate.tokens, estimate.last_usage_index), (2_002, Some(3)));
}

#[test]
fn max_is_opt_in_and_holes_clamp_upward() {
    let mut model = Model { reasoning: true, ..model() };
    use ThinkingLevel::*;
    assert_eq!(supported_thinking_levels(&model), [Off, Minimal, Low, Medium, High]);
    assert_eq!(clamp_thinking_level(&model, Max), High);
    model.thinking_level_map =
        Some([("xhigh".to_string(), None), ("max".to_string(), Some("max".to_string()))].into_iter().collect());
    assert_eq!(supported_thinking_levels(&model), [Off, Minimal, Low, Medium, High, Max]);
    assert_eq!(clamp_thinking_level(&model, Xhigh), Max);
    model.thinking_level_map = Some([("off".to_string(), None)].into_iter().collect());
    assert_eq!(clamp_thinking_level(&model, Off), Minimal);
    assert_eq!(supported_thinking_levels(&Model::default()), [Off]);
}

#[test]
fn sampling_params_merge_model_level_and_request_values() {
    let mut model = model();
    assert_eq!(resolve_sampling_params(&model, ThinkingLevel::Off, None), None);
    model.sampling_params = json!({"top_p": 0.9, "top_k": 20}).as_object().cloned();
    model.sampling_params_by_thinking_level =
        Some([("off".to_string(), json!({"top_k": 40}).as_object().cloned().unwrap())].into_iter().collect());
    let request = json!({"top_p": 0.5, "min_p": 0.1});
    let merged = resolve_sampling_params(&model, ThinkingLevel::High, request.as_object()).unwrap();
    assert_eq!(serde_json::Value::Object(merged), json!({"top_p": 0.5, "top_k": 40, "min_p": 0.1}));
}

#[test]
fn thinking_budgets_fit_the_output_cap() {
    let custom = ThinkingBudgets { high: Some(30_000), ..Default::default() };
    assert_eq!(thinking_budget_for_level(ThinkingLevel::Max, None), 16_384);
    assert_eq!(thinking_budget_for_level(ThinkingLevel::Xhigh, Some(&custom)), 30_000);
    assert_eq!(thinking_budget_for_level(ThinkingLevel::Low, Some(&custom)), 2_048);
    assert_eq!(adjust_max_tokens_for_thinking(Some(4_000), 64_000, ThinkingLevel::Medium, None), (12_192, 8_192));
    assert_eq!(adjust_max_tokens_for_thinking(None, 8_000, ThinkingLevel::High, None), (8_000, 6_976));
    assert_eq!(adjust_max_tokens_for_thinking(Some(1_000), 64_000, ThinkingLevel::Low, None), (3_048, 2_048));
}

#[test]
fn cache_retention_and_prompt_cache_keys() {
    let env = [("PI_CACHE_RETENTION".to_string(), "long".to_string())].into_iter().collect();
    assert_eq!(resolve_cache_retention(None, &env), CacheRetention::Long);
    assert_eq!(resolve_cache_retention(Some(CacheRetention::None), &env), CacheRetention::None);
    assert_eq!(clamp_openai_prompt_cache_key(&"é".repeat(70)).chars().count(), 64);
    assert_eq!(clamp_openai_prompt_cache_key("session"), "session");
}
