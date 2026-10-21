//! OpenRouter's per-key model list, `GET /models/user`, and its price conversion (upstream
//! `providers/openrouter.ts` and `providers/openrouter-api.ts`).

use std::collections::HashMap;

use serde_json::Value;

use super::{Fetched, ModelListContext, get_json};
use crate::models::is_chat_model;
use crate::types::{Model, ModelCost, ModelCostTier};

pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// One $/token price as $/1M tokens. Missing, invalid and negative prices use `fallback`:
/// OpenRouter reports -1 for router models such as `openrouter/auto`.
fn per_million(value: Option<&Value>, fallback: f64) -> f64 {
    let per_token = match value {
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    };
    match per_token.filter(|price| *price >= 0.0) {
        Some(price) => format!("{:.6}", price * 1_000_000.0).parse().unwrap_or(fallback),
        None => fallback,
    }
}

/// OpenRouter's $/token prices as pi's $/1M tokens. Prompt-length overrides become request-wide
/// tiers; time-of-day overrides are skipped because [`ModelCost`] cannot express them.
pub fn openrouter_cost(pricing: Option<&Value>) -> ModelCost {
    let rate = |object: Option<&Value>, name: &str, fallback: f64| {
        per_million(object.and_then(|object| object.get(name)), fallback)
    };
    let (input, output) = (rate(pricing, "prompt", 0.0), rate(pricing, "completion", 0.0));
    let (cache_read, cache_write) = (rate(pricing, "input_cache_read", 0.0), rate(pricing, "input_cache_write", 0.0));
    let overrides = pricing.and_then(|pricing| pricing.get("overrides")).and_then(Value::as_array);
    let tiers = overrides.into_iter().flatten().filter_map(|tier| {
        let above = tier.get("min_prompt_tokens").and_then(Value::as_f64)?;
        if ["utc_start", "utc_end", "utc_days"].iter().any(|name| tier.get(*name).is_some()) {
            return None;
        }
        Some(ModelCostTier {
            input_tokens_above: above.max(0.0) as u64,
            input: rate(Some(tier), "prompt", input),
            output: rate(Some(tier), "completion", output),
            cache_read: rate(Some(tier), "input_cache_read", cache_read),
            cache_write: rate(Some(tier), "input_cache_write", cache_write),
        })
    });
    ModelCost { input, output, cache_read, cache_write, tiers: tiers.collect() }
}

/// A limit OpenRouter reports; absent, null and 0 mean missing, like the generator's fallback.
fn limit(entry: &Value, pointer: &str) -> Option<u64> {
    entry.pointer(pointer).and_then(Value::as_f64).filter(|value| *value > 0.0).map(|value| value as u64)
}

/// Restrict an OpenRouter catalog to the chat models the key may use, with the key's limits and
/// prices; image and classifier models stay. The configured base URL applies regional filters,
/// and a 401, 403 or 404 there (for example from a proxy) returns the catalog unchanged.
pub async fn personalize_openrouter_catalog(
    catalog: Vec<Model>,
    context: &ModelListContext,
) -> Result<Vec<Model>, String> {
    let Some(key) = context.key() else { return Ok(catalog) };
    let url = format!("{}/models/user", context.base(OPENROUTER_BASE_URL));
    let invalid = || "Invalid OpenRouter model list response".to_string();
    let body = match get_json(context, "OpenRouter", &url, &[("authorization", format!("Bearer {key}"))]).await? {
        Fetched::Json(body) => body,
        Fetched::Rejected => return Ok(catalog),
        Fetched::Invalid => return Err(invalid()),
    };
    let data = body.get("data").and_then(Value::as_array).ok_or_else(invalid)?;
    let allowed: HashMap<&str, &Value> =
        data.iter().filter_map(|entry| Some((entry.get("id")?.as_str()?, entry))).collect();
    let personalized = catalog.into_iter().filter_map(|model| {
        if !is_chat_model(&model) {
            return Some(model);
        }
        let entry = *allowed.get(model.id.as_str())?;
        let context_window = limit(entry, "/top_provider/context_length").or_else(|| limit(entry, "/context_length"));
        let max_tokens = limit(entry, "/top_provider/max_completion_tokens");
        let cost = match entry.get("pricing") {
            Some(pricing) if !pricing.is_null() => openrouter_cost(Some(pricing)),
            _ => model.cost.clone(),
        };
        Some(Model {
            context_window: context_window.unwrap_or(model.context_window),
            max_tokens: max_tokens.unwrap_or(model.max_tokens),
            cost,
            ..model
        })
    });
    Ok(personalized.collect())
}
