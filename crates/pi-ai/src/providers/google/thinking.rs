//! Gemini thinking configuration: discrete `thinkingLevel` for Gemini 3 and Gemma 4,
//! token `thinkingBudget` for older models.

use serde_json::{Value, json};

use crate::registry::StreamOptions;
use crate::simple_options::{MappedLevel, clamp_thinking_level, mapped_thinking_level};
use crate::types::{Model, ThinkingLevel};

/// Matches `gemini-3-pro`, `gemini-3.1-flash`, and similar ids.
fn is_gemini3_pro_or_flash(id: &str) -> bool {
    id.match_indices("gemini-3").any(|(index, matched)| {
        let rest = &id[index + matched.len()..];
        let rest = match rest.strip_prefix('.') {
            Some(minor) => {
                let digits = minor.chars().take_while(char::is_ascii_digit).count();
                if digits == 0 {
                    return false;
                }
                &minor[digits..]
            }
            None => rest,
        };
        rest.starts_with("-pro") || rest.starts_with("-flash")
    })
}

/// Whether the model uses Gemini's discrete `thinkingLevel` instead of `thinkingBudget`.
pub(crate) fn uses_thinking_level(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    is_gemini3_pro_or_flash(&id)
        || id == "gemini-flash-latest"
        || id == "gemini-flash-lite-latest"
        || id.contains("gemma-4")
        || id.contains("gemma4")
}

/// A supported pi level, or the model's mapping of it, as a standard Google level.
pub(crate) fn resolve_google_thinking_level(model: &Model, level: ThinkingLevel) -> Result<ThinkingLevel, String> {
    let mapped = mapped_thinking_level(model, level);
    let resolved = match mapped {
        MappedLevel::Value(value) => value.to_lowercase(),
        _ => level.as_str().to_string(),
    };
    match resolved.as_str() {
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        _ => {
            let shown = match mapped {
                MappedLevel::Default => "undefined",
                MappedLevel::Unsupported => "null",
                MappedLevel::Value(value) => value,
            };
            Err(format!(
                "Unsupported Google thinking level mapping for {}/{}: {} -> {shown}",
                model.provider,
                model.id,
                level.as_str()
            ))
        }
    }
}

fn api_level(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "MINIMAL",
        ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium => "MEDIUM",
        _ => "HIGH",
    }
}

fn disabled_config(model: &Model) -> Result<Value, String> {
    if !uses_thinking_level(model) {
        return Ok(json!({"thinkingBudget": 0}));
    }
    match clamp_thinking_level(model, ThinkingLevel::Off) {
        ThinkingLevel::Off => Ok(json!({"thinkingBudget": 0})),
        fallback => Ok(json!({"thinkingLevel": api_level(resolve_google_thinking_level(model, fallback)?)})),
    }
}

/// Token budget for `level`; `-1` asks the model to choose dynamically.
fn budget(model: &Model, level: ThinkingLevel, options: &StreamOptions, vertex: bool) -> i64 {
    let custom = options.thinking_budgets.and_then(|budgets| match level {
        ThinkingLevel::Minimal => budgets.minimal,
        ThinkingLevel::Low => budgets.low,
        ThinkingLevel::Medium => budgets.medium,
        _ => budgets.high,
    });
    if let Some(custom) = custom {
        return custom as i64;
    }
    let table: [i64; 4] = if model.id.contains("2.5-pro") {
        [128, 2048, 8192, 32768]
    } else if model.id.contains("2.5-flash-lite") && !vertex {
        [512, 2048, 8192, 24576]
    } else if model.id.contains("2.5-flash") {
        [128, 2048, 8192, 24576]
    } else {
        return -1;
    };
    match level {
        ThinkingLevel::Minimal => table[0],
        ThinkingLevel::Low => table[1],
        ThinkingLevel::Medium => table[2],
        _ => table[3],
    }
}

/// `thinkingConfig` for the request. Reasoning models get an explicit disabled config when
/// thinking is off; non-reasoning models get none.
pub(crate) fn thinking_config(model: &Model, options: &StreamOptions, vertex: bool) -> Result<Option<Value>, String> {
    if !model.reasoning {
        return Ok(None);
    }
    let Some(requested) = options.reasoning.filter(|level| *level != ThinkingLevel::Off) else {
        return disabled_config(model).map(Some);
    };
    let clamped = clamp_thinking_level(model, requested);
    if clamped == ThinkingLevel::Off {
        return disabled_config(model).map(Some);
    }
    let resolved = resolve_google_thinking_level(model, clamped)?;
    Ok(Some(if uses_thinking_level(model) {
        json!({"includeThoughts": true, "thinkingLevel": api_level(resolved)})
    } else {
        json!({"includeThoughts": true, "thinkingBudget": budget(model, resolved, options, vertex)})
    }))
}
