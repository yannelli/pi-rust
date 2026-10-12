//! Bedrock model capability checks. Both the id and the name are matched, so application
//! inference profiles whose ARNs hide the model still resolve through `Model.name`.

use std::collections::BTreeMap;

use crate::providers::provider_env::provider_env_value;
use crate::types::{Model, ThinkingLevel};

fn candidates(model: &Model) -> Vec<String> {
    let mut values = vec![model.id.as_str()];
    if !model.name.is_empty() {
        values.push(model.name.as_str());
    }
    values
        .into_iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let mut dashed = String::with_capacity(lower.len());
            let mut in_separator_run = false;
            for c in lower.chars() {
                let separator = c.is_whitespace() || matches!(c, '_' | '.' | ':');
                if !separator {
                    dashed.push(c);
                } else if !in_separator_run {
                    dashed.push('-');
                }
                in_separator_run = separator;
            }
            [lower, dashed]
        })
        .collect()
}

fn any_contains(model: &Model, needles: &[&str]) -> bool {
    candidates(model).iter().any(|candidate| needles.iter().any(|needle| candidate.contains(needle)))
}

pub(super) fn is_claude(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    ["anthropic.claude", "anthropic/claude"].iter().any(|needle| id.contains(needle) || name.contains(needle))
        || name.contains("claude")
}

pub(super) fn supports_adaptive_thinking(model: &Model) -> bool {
    any_contains(model, &["opus-4-6", "opus-4-7", "opus-4-8", "opus-5", "sonnet-4-6", "sonnet-5", "haiku-5", "fable-5"])
}

fn supports_native_xhigh(model: &Model) -> bool {
    any_contains(model, &["opus-4-7", "opus-4-8", "opus-5", "sonnet-5", "haiku-5", "fable-5"])
}

/// Opus 4.6 and Sonnet 4.6 reject `thinking.block_binding`.
pub(super) fn supports_block_binding(model: &Model) -> bool {
    supports_native_xhigh(model)
}

fn mapped_level(model: &Model, level: ThinkingLevel) -> Option<String> {
    model.thinking_level_map.as_ref()?.get(level.as_str()).cloned().flatten()
}

/// Claude adaptive `output_config.effort` for a pi thinking level.
pub(super) fn map_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::Xhigh && supports_native_xhigh(model) {
        return "xhigh".into();
    }
    if let Some(mapped) = mapped_level(model, level) {
        return mapped;
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    }
    .into()
}

/// `reasoning_effort` for gpt-oss, which only accepts low, medium, and high.
pub(super) fn gpt_oss_effort(model: &Model, level: ThinkingLevel) -> Option<&'static str> {
    any_contains(model, &["gpt-oss"]).then_some(match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low | ThinkingLevel::Off => "low",
        ThinkingLevel::Medium => "medium",
        _ => "high",
    })
}

/// Nested `reasoning.effort` for OpenAI GPT models, which reject `minimal`.
pub(super) fn gpt_effort(model: &Model, level: ThinkingLevel) -> Option<String> {
    if !any_contains(model, &["gpt-"]) {
        return None;
    }
    Some(mapped_level(model, level).unwrap_or_else(|| {
        match level {
            ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
        .into()
    }))
}

/// Claude 3.5 Haiku, 3.7 Sonnet, 4.x, and 5 models; `AWS_BEDROCK_FORCE_CACHE=1` for others.
pub(super) fn supports_prompt_caching(model: &Model, env: &BTreeMap<String, String>) -> bool {
    let all = candidates(model);
    if !all.iter().any(|candidate| candidate.contains("claude")) {
        return provider_env_value("AWS_BEDROCK_FORCE_CACHE", env).as_deref() == Some("1");
    }
    any_contains(model, &["fable-5", "opus-5", "sonnet-5", "haiku-5", "-4-", "claude-3-7-sonnet", "claude-3-5-haiku"])
}

/// GovCloud rejects the Claude `thinking.display` field and block binding.
pub(super) fn is_gov_cloud(model: &Model, configured_region: Option<&str>) -> bool {
    if configured_region.is_some_and(|region| region.to_lowercase().starts_with("us-gov-")) {
        return true;
    }
    let id = model.id.to_lowercase();
    id.starts_with("us-gov.") || id.starts_with("arn:aws-us-gov:")
}
