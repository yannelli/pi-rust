//! Thinking parameters per `compat.thinkingFormat`, chat template values with `$var`
//! substitution, and the reasoning token budget. Port of the thinking part of upstream `buildParams`.

use serde_json::{Map, Value, json};

use super::compat::Compat;
use crate::registry::StreamOptions;
use crate::simple_options::{
    MappedLevel, clamp_thinking_budget_to_answer_room, clamp_thinking_level, mapped_thinking_level,
    thinking_budget_for_level,
};
use crate::types::{Model, ThinkingLevel};

/// The requested effort after clamping to the model's levels; `None` when thinking is off.
pub(super) fn reasoning_effort(model: &Model, options: &StreamOptions) -> Option<ThinkingLevel> {
    let level = clamp_thinking_level(model, options.reasoning?);
    (level != ThinkingLevel::Off).then_some(level)
}

/// The budget for `effort` from `options.thinking_budgets`, leaving answer room under `ceiling`.
pub(super) fn thinking_budget(
    model: &Model,
    options: &StreamOptions,
    effort: Option<ThinkingLevel>,
    ceiling: u64,
) -> Option<u64> {
    let effort = effort.filter(|_| model.reasoning)?;
    let budget = thinking_budget_for_level(effort, options.thinking_budgets.as_ref());
    Some(clamp_thinking_budget_to_answer_room(budget, ceiling)).filter(|budget| *budget > 0)
}

/// The mapped provider value, falling back to the level name when unmapped or `null`.
fn mapped_or_level(model: &Model, level: ThinkingLevel) -> String {
    match mapped_thinking_level(model, level) {
        MappedLevel::Value(value) => value.to_string(),
        MappedLevel::Default | MappedLevel::Unsupported => level.as_str().to_string(),
    }
}

/// The mapped provider value; the level name when unmapped, nothing when `null`.
fn mapped_value(model: &Model, level: ThinkingLevel) -> Option<String> {
    match mapped_thinking_level(model, level) {
        MappedLevel::Value(value) => Some(value.to_string()),
        MappedLevel::Default => Some(level.as_str().to_string()),
        MappedLevel::Unsupported => None,
    }
}

/// The value for a disabled request: the mapped `off` value, `fallback` when unmapped, and
/// nothing when the map marks `off` unsupported.
fn off_value(model: &Model, fallback: Option<&str>) -> Option<String> {
    match mapped_thinking_level(model, ThinkingLevel::Off) {
        MappedLevel::Value(value) => Some(value.to_string()),
        MappedLevel::Default => fallback.map(str::to_string),
        MappedLevel::Unsupported => None,
    }
}

/// The effort for `effort`, or the mapped `off` value when thinking is off.
fn effort_or_off(model: &Model, effort: Option<ThinkingLevel>) -> Option<String> {
    match effort {
        Some(level) => mapped_value(model, level),
        None => off_value(model, None),
    }
}

/// Insert the thinking fields for `compat.thinking_format` into `body`.
pub(super) fn apply_thinking(
    model: &Model,
    compat: &Compat,
    effort: Option<ThinkingLevel>,
    budget: Option<u64>,
    body: &mut Map<String, Value>,
) {
    if !model.reasoning {
        return;
    }
    let enabled = effort.is_some();
    let effort_allowed = compat.supports_reasoning_effort;
    let mut set = |key: &str, value: Value| {
        body.insert(key.to_string(), value);
    };
    match compat.thinking_format.as_str() {
        "zai" => {
            set(
                "thinking",
                if enabled { json!({"type": "enabled", "clear_thinking": false}) } else { json!({"type": "disabled"}) },
            );
            if let (Some(level), true) = (effort, effort_allowed)
                && let Some(value) = mapped_value(model, level)
            {
                set("reasoning_effort", json!(value));
            }
        }
        "qwen" => {
            set("enable_thinking", json!(enabled));
            if let (Some(level), true) = (effort, effort_allowed) {
                set("reasoning_effort", json!(mapped_or_level(model, level)));
            }
        }
        "qwen-chat-template" => {
            set("chat_template_kwargs", json!({"enable_thinking": enabled, "preserve_thinking": true}));
        }
        "chat-template" => {
            if let Some(kwargs) = chat_template_values(model, effort, &compat.chat_template_kwargs, budget) {
                set("chat_template_kwargs", Value::Object(kwargs));
            }
        }
        "baseten" => {
            if let Some(args) = chat_template_values(model, effort, &compat.chat_template_args, budget) {
                set("chat_template_args", Value::Object(args));
            }
            if let (Some(value), true) = (effort_or_off(model, effort), effort_allowed) {
                set("reasoning_effort", json!(value));
            }
        }
        "deepseek" => {
            if enabled {
                set("thinking", json!({"type": "enabled"}));
            } else if mapped_thinking_level(model, ThinkingLevel::Off) != MappedLevel::Unsupported {
                set("thinking", json!({"type": "disabled"}));
            }
            if let (Some(level), true) = (effort, effort_allowed) {
                set("reasoning_effort", json!(mapped_or_level(model, level)));
            }
        }
        "openrouter" => {
            let value = match effort {
                Some(level) => Some(mapped_or_level(model, level)),
                None => off_value(model, Some("none")),
            };
            if let Some(value) = value {
                set("reasoning", json!({"effort": value}));
            }
        }
        "ant-ling" if enabled => {
            if let Some(MappedLevel::Value(value)) = effort.map(|level| mapped_thinking_level(model, level)) {
                set("reasoning", json!({"effort": value}));
            }
        }
        "together" => {
            set("reasoning", json!({"enabled": enabled}));
            if let (Some(level), true) = (effort, effort_allowed) {
                set("reasoning_effort", json!(mapped_or_level(model, level)));
            }
        }
        "string-thinking" => {
            let value = match effort {
                Some(level) => Some(mapped_or_level(model, level)),
                None => off_value(model, Some("none")),
            };
            if let Some(value) = value {
                set("thinking", json!(value));
            }
        }
        _ if !effort_allowed => {}
        _ => {
            let value = match effort {
                Some(level) => Some(mapped_or_level(model, level)),
                None => off_value(model, None),
            };
            if let Some(value) = value {
                set("reasoning_effort", json!(value));
            }
        }
    }
}

/// Resolve `chatTemplateKwargs` or `chatTemplateArgs`; `None` when nothing remains.
fn chat_template_values(
    model: &Model,
    effort: Option<ThinkingLevel>,
    values: &Map<String, Value>,
    budget: Option<u64>,
) -> Option<Map<String, Value>> {
    let resolved: Map<String, Value> = values
        .iter()
        .filter_map(|(key, value)| Some((key.clone(), chat_template_value(model, effort, value, budget)?)))
        .collect();
    (!resolved.is_empty()).then_some(resolved)
}

/// A literal passes through; `{"$var": ...}` takes `thinking.enabled`, `thinking.budget`, or
/// the effort, and `omitWhenOff` drops the key when thinking is off.
fn chat_template_value(
    model: &Model,
    effort: Option<ThinkingLevel>,
    value: &Value,
    budget: Option<u64>,
) -> Option<Value> {
    let Some(object) = value.as_object() else { return Some(value.clone()) };
    let truthy = |value: Option<&Value>| value.is_some_and(|value| !matches!(value, Value::Null | Value::Bool(false)));
    if effort.is_none() && truthy(object.get("omitWhenOff")) {
        return None;
    }
    match object.get("$var").and_then(Value::as_str) {
        Some("thinking.enabled") => Some(json!(effort.is_some())),
        Some("thinking.budget") => budget.map(|budget| json!(budget)),
        _ => effort_or_off(model, effort).map(Value::String),
    }
}
