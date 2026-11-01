//! The `configOptions` of `fake-acp-agent`, picked by `FAKE_ACP_CONFIG`: the default set, `numeric`
//! (effort without a category and numeric values), `grouped`, or `legacy` (no config options).

use serde_json::{Value, json};

fn model_option(model: &str, options: Value) -> Value {
    json!({"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
        "options": options})
}

fn models() -> Value {
    json!([{"value": "fast", "name": "Fast"}, {"value": "smart", "name": "Smart"}])
}

/// The permission values of the `numeric` variant, whose `permission` option is the agent mode.
pub const PERMISSIONS: [&str; 2] = ["ask", "auto"];

/// The options of `variant`; `None` for an agent that only has `modes` and `models`.
pub fn options(variant: &str, model: &str, effort: &str, mode: &str) -> Option<Value> {
    Some(match variant {
        "legacy" => return None,
        "numeric" => json!([
            model_option(model, models()),
            {"id": "effortLevel", "name": "Effort", "type": "select", "currentValue": effort,
             "options": [{"value": "1", "name": "Low"}, {"value": "2", "name": "Medium"},
                {"value": "3", "name": "High"}]},
            {"id": "permission", "name": "Permissions", "category": "_custom", "type": "select", "currentValue": mode,
             "options": [{"value": "ask", "name": "Ask"}, {"value": "auto", "name": "Auto"}]},
            {"id": "verbose", "name": "Verbose", "type": "boolean", "currentValue": false},
        ]),
        "grouped" => json!([
            model_option(model, json!([
                {"group": "quick", "name": "Quick", "options": [{"value": "fast", "name": "Fast"}]},
                {"group": "deep", "name": "Deep", "options": [{"value": "smart", "name": "Smart"}]}])),
            {"id": "effort", "name": "Speed", "category": "_kiro_speed", "type": "select", "currentValue": effort,
             "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
        ]),
        _ => json!([
            model_option(model, models()),
            {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select", "currentValue": effort,
             "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
        ]),
    })
}

/// The `models` field of an agent without config options.
pub fn legacy_models(model: &str) -> Value {
    json!({"currentModelId": model, "availableModels": [{"modelId": "fast", "name": "Fast"},
        {"modelId": "smart", "name": "Smart"}]})
}

/// The effort value an agent of `variant` starts with.
pub fn initial_effort(variant: &str) -> &'static str {
    if variant == "numeric" { "2" } else { "low" }
}
