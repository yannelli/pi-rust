//! OpenRouter-reported billing, ported from upstream `utils/openrouter-cost.ts`. OpenRouter adds
//! `cost`, `is_byok`, and `cost_details` to streamed usage at runtime.

use serde_json::Value;

use crate::types::{Model, Usage};

fn valid_cost(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|cost| cost.is_finite() && *cost >= 0.0)
}

pub(crate) fn is_openrouter_model(model: &Model) -> bool {
    model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
}

/// Replace the catalog total with OpenRouter's billed cost when the response contains complete, valid accounting.
pub(crate) fn apply_openrouter_reported_cost(usage: &mut Usage, reported: &Value) {
    let Some(cost) = valid_cost(reported.get("cost")) else { return };
    let total = match reported.get("is_byok").and_then(Value::as_bool) {
        Some(false) => cost,
        Some(true) => {
            let Some(details) = reported.get("cost_details").filter(|details| details.is_object()) else { return };
            let upstream = match details.get("upstream_inference_cost") {
                Some(Value::Null) => {
                    let prompt = valid_cost(details.get("upstream_inference_prompt_cost"));
                    let completions = valid_cost(details.get("upstream_inference_completions_cost"));
                    let (Some(prompt), Some(completions)) = (prompt, completions) else { return };
                    prompt + completions
                }
                other => {
                    let Some(upstream) = valid_cost(other) else { return };
                    upstream
                }
            };
            cost + upstream
        }
        None => return,
    };
    usage.cost.total = total;
    usage.cost.total_from_provider = Some(true);
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::types::Cost;

    fn usage() -> Usage {
        let cost = Cost { input: 0.25, output: 0.75, total: 1.0, ..Cost::default() };
        Usage { input: 1, output: 1, total_tokens: 2, cost, ..Usage::default() }
    }

    #[test]
    fn recognizes_the_built_in_provider_and_custom_providers_on_openrouter() {
        let model = |provider: &str, base_url: &str| Model {
            provider: provider.into(),
            base_url: base_url.into(),
            ..Model::default()
        };
        assert!(is_openrouter_model(&model("openrouter", "https://example.com")));
        assert!(is_openrouter_model(&model("openrouter-work", "https://openrouter.ai/api/v1")));
        assert!(!is_openrouter_model(&model("custom", "https://example.com/v1")));
    }

    #[test]
    fn uses_the_reported_cost_for_non_byok_requests() {
        let mut value = usage();
        let details = json!({"upstream_inference_cost": 0, "upstream_inference_prompt_cost": 0,
            "upstream_inference_completions_cost": 0});
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0, "is_byok": false, "cost_details": details}));
        assert_eq!((value.cost.total, value.cost.total_from_provider), (0.0, Some(true)));
    }

    #[test]
    fn adds_the_upstream_inference_cost_for_byok_requests() {
        let mut value = usage();
        let details = json!({"upstream_inference_cost": 0.9, "upstream_inference_prompt_cost": 0.6,
            "upstream_inference_completions_cost": 0.3});
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0.1, "is_byok": true, "cost_details": details}));
        assert_eq!((value.cost.total, value.cost.total_from_provider), (1.0, Some(true)));
    }

    #[test]
    fn uses_upstream_cost_components_when_their_total_is_null() {
        let mut value = usage();
        let details = json!({"upstream_inference_cost": null, "upstream_inference_prompt_cost": 0.6,
            "upstream_inference_completions_cost": 0.3});
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0.1, "is_byok": true, "cost_details": details}));
        assert!((value.cost.total - 1.0).abs() < 1e-9);
        assert_eq!(value.cost.total_from_provider, Some(true));
    }

    #[test]
    fn keeps_the_catalog_total_when_accounting_is_incomplete() {
        let mut value = usage();
        value.cost.total_from_provider = Some(false);
        apply_openrouter_reported_cost(&mut value, &json!({"cost": null, "is_byok": false, "cost_details": null}));
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0.1, "is_byok": true, "cost_details": null}));
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0.1, "is_byok": true, "cost_details": {}}));
        apply_openrouter_reported_cost(&mut value, &json!({"cost": -1, "is_byok": false}));
        apply_openrouter_reported_cost(&mut value, &json!({"cost": 0.1}));
        assert_eq!((value.cost.total, value.cost.total_from_provider), (1.0, Some(false)));
    }
}
