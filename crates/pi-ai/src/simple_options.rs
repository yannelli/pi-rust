//! Request option helpers shared by adapters: output caps, thinking budgets and levels,
//! sampling parameters, and cache retention. Port of upstream `api/simple-options.ts`,
//! `api/openai-prompt-cache.ts`, and the thinking level helpers of `models.ts`.

use std::collections::BTreeMap;

use crate::estimate::estimate_context_tokens;
use crate::registry::{CacheRetention, ThinkingBudgets};
use crate::types::{JsonObject, Message, Model, ThinkingLevel};

/// Tokens kept free below the context window when capping the output.
pub const CONTEXT_SAFETY_TOKENS: u64 = 4096;
/// Tokens always left for the answer when a thinking budget shares the response ceiling.
pub const MIN_ANSWER_TOKENS: u64 = 1024;
pub const OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

pub const DEFAULT_THINKING_BUDGETS: ThinkingBudgets =
    ThinkingBudgets { minimal: Some(1024), low: Some(2048), medium: Some(8192), high: Some(16384) };

/// Cap `max_tokens` so the estimated context plus the output fits the context window.
pub fn clamp_max_tokens_to_context(model: &Model, messages: &[Message], max_tokens: u64) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(1);
    }
    let used = estimate_context_tokens(messages).tokens + CONTEXT_SAFETY_TOKENS;
    let available = model.context_window.saturating_sub(used).max(1);
    max_tokens.min(available)
}

/// How `model.thinking_level_map` treats a level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MappedLevel<'a> {
    /// No entry: the provider default applies.
    Default,
    /// `null`: the level is unsupported.
    Unsupported,
    /// A provider-native value.
    Value(&'a str),
}

pub fn mapped_thinking_level(model: &Model, level: ThinkingLevel) -> MappedLevel<'_> {
    match model.thinking_level_map.as_ref().and_then(|map| map.get(level.as_str())) {
        None => MappedLevel::Default,
        Some(None) => MappedLevel::Unsupported,
        Some(Some(value)) => MappedLevel::Value(value),
    }
}

/// The provider value for `level`: the mapped value, or the level name when unmapped.
/// `None` when the map marks the level unsupported.
pub fn provider_thinking_value(model: &Model, level: ThinkingLevel) -> Option<String> {
    match mapped_thinking_level(model, level) {
        MappedLevel::Default => Some(level.as_str().to_string()),
        MappedLevel::Unsupported => None,
        MappedLevel::Value(value) => Some(value.to_string()),
    }
}

/// Levels `model` accepts: `off` only without reasoning; `xhigh` and `max` need a map entry.
pub fn supported_thinking_levels(model: &Model) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return vec![ThinkingLevel::Off];
    }
    ThinkingLevel::ALL
        .into_iter()
        .filter(|level| match mapped_thinking_level(model, *level) {
            MappedLevel::Unsupported => false,
            MappedLevel::Default => !matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max),
            MappedLevel::Value(_) => true,
        })
        .collect()
}

/// The nearest supported level: the requested one, else the next higher, else the next lower.
pub fn clamp_thinking_level(model: &Model, level: ThinkingLevel) -> ThinkingLevel {
    let available = supported_thinking_levels(model);
    if available.contains(&level) {
        return level;
    }
    let all = ThinkingLevel::ALL;
    let index = all.iter().position(|candidate| *candidate == level).unwrap_or_default();
    all[index..]
        .iter()
        .chain(all[..index].iter().rev())
        .find(|candidate| available.contains(candidate))
        .copied()
        .unwrap_or(ThinkingLevel::Off)
}

/// Anthropic models with fast mode (`speed: "fast"`) on the first-party API.
const ANTHROPIC_FAST_MODELS: [&str; 3] = ["claude-opus-5-5", "claude-opus-5", "claude-opus-4-8"];

/// Whether `model` has a fast mode (pi-rust extension). A `compat.fastMode` bool decides;
/// otherwise Anthropic's fast models, Codex models that offer the `priority` service tier, and
/// Cursor models with fast variants have one.
pub fn supports_fast(model: &Model) -> bool {
    let compat = model.compat.as_ref();
    if let Some(fast) = compat.and_then(|compat| compat.get("fastMode")).and_then(serde_json::Value::as_bool) {
        return fast;
    }
    match model.api.as_str() {
        "anthropic-messages" => model.provider == "anthropic" && ANTHROPIC_FAST_MODELS.contains(&model.id.as_str()),
        "openai-codex-responses" => compat
            .and_then(|compat| compat.get("serviceTiers"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tiers| tiers.iter().any(|tier| tier == "priority")),
        "cursor-agent" => {
            compat.is_some_and(|compat| compat.get("cursorFast").is_some_and(serde_json::Value::is_object))
        }
        _ => false,
    }
}

/// Model sampling parameters, then the ones for the effective thinking level, then the request's.
pub fn resolve_sampling_params(
    model: &Model,
    level: ThinkingLevel,
    request: Option<&JsonObject>,
) -> Option<JsonObject> {
    let effective = clamp_thinking_level(model, level);
    let by_level = model.sampling_params_by_thinking_level.as_ref().and_then(|map| map.get(effective.as_str()));
    if model.sampling_params.is_none() && by_level.is_none() && request.is_none() {
        return None;
    }
    let mut merged = JsonObject::new();
    for source in [model.sampling_params.as_ref(), by_level, request].into_iter().flatten() {
        merged.extend(source.iter().map(|(key, value)| (key.clone(), value.clone())));
    }
    Some(merged)
}

/// `xhigh` and `max` map to `high` for budget-based thinking.
pub fn clamp_reasoning(level: ThinkingLevel) -> ThinkingLevel {
    match level {
        ThinkingLevel::Xhigh | ThinkingLevel::Max => ThinkingLevel::High,
        level => level,
    }
}

/// Token budget for `level`, from `custom` budgets over the defaults.
pub fn thinking_budget_for_level(level: ThinkingLevel, custom: Option<&ThinkingBudgets>) -> u64 {
    let pick = |budgets: &ThinkingBudgets| match clamp_reasoning(level) {
        ThinkingLevel::Off | ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        _ => budgets.high,
    };
    custom.and_then(pick).or_else(|| pick(&DEFAULT_THINKING_BUDGETS)).unwrap_or(1024)
}

/// Cap a thinking budget so at least [`MIN_ANSWER_TOKENS`] remain under `ceiling`.
pub fn clamp_thinking_budget_to_answer_room(budget: u64, ceiling: u64) -> u64 {
    budget.min(ceiling.saturating_sub(MIN_ANSWER_TOKENS))
}

/// `(max_tokens, thinking_budget)` when thinking shares the output cap. Without a caller cap
/// the model cap applies and thinking fits inside it.
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    level: ThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> (u64, u64) {
    let mut budget = thinking_budget_for_level(level, custom);
    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + budget).min(model_max_tokens),
    };
    if max_tokens <= budget {
        budget = clamp_thinking_budget_to_answer_room(budget, max_tokens);
    }
    (max_tokens, budget)
}

/// A provider-scoped value from `env`, else the process environment. Empty values count as unset.
pub fn provider_env_value(name: &str, env: &BTreeMap<String, String>) -> Option<String> {
    env.get(name)
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|value| !value.is_empty()))
}

/// The requested retention, else `long` when `PI_CACHE_RETENTION=long`, else `short`.
pub fn resolve_cache_retention(requested: Option<CacheRetention>, env: &BTreeMap<String, String>) -> CacheRetention {
    requested.unwrap_or_else(|| match provider_env_value("PI_CACHE_RETENTION", env).as_deref() {
        Some("long") => CacheRetention::Long,
        _ => CacheRetention::Short,
    })
}

/// OpenAI rejects prompt cache keys longer than 64 characters.
pub fn clamp_openai_prompt_cache_key(key: &str) -> String {
    key.chars().take(OPENAI_PROMPT_CACHE_KEY_MAX_LENGTH).collect()
}

#[cfg(test)]
#[path = "simple_options_tests.rs"]
mod tests;
