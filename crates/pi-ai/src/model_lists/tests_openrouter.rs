//! Port of upstream `openrouter-user-models.test.ts`, against a local server.

use serde_json::{Value, json};

use super::*;
use crate::providers::http_mock::{Reply, RouteServer};
use crate::types::ModelCost;

fn chat(id: &str) -> Model {
    serde_json::from_value(json!({"id": id, "name": id, "api": "openai-completions", "provider": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1", "input": ["text"], "contextWindow": 200_000, "maxTokens": 50_000,
        "cost": {"input": 1, "output": 2, "cacheRead": 0.1, "cacheWrite": 0}}))
    .unwrap()
}

fn typed(kind: &str, id: &str) -> Model {
    serde_json::from_value(json!({"type": kind, "id": id, "name": id, "api": "openrouter-images",
        "provider": "openrouter", "baseUrl": "https://openrouter.ai/api/v1", "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}}))
    .unwrap()
}

pub(super) fn context(server: &RouteServer, key: Option<&str>) -> ModelListContext {
    ModelListContext {
        base_url: Some(format!("{}/api/v1/", server.base_url)),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        ..ModelListContext::new(key.map(String::from))
    }
}

pub(super) async fn serve(status: u16, body: Value) -> RouteServer {
    RouteServer::start(move |_| Reply::json(status, &body)).await
}

fn ids(models: &[Model]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

#[tokio::test]
async fn keeps_only_the_keys_chat_models_with_the_keys_limits_and_prices() {
    let data = json!([
        {"id": "listed", "context_length": 1_000_000,
         "top_provider": {"context_length": 262_144, "max_completion_tokens": 32_768},
         "pricing": {"prompt": "0.000003", "completion": "0.000015"}},
        // A missing limit keeps the catalog value.
        {"id": "no-limits", "context_length": 0, "top_provider": {"context_length": null, "max_completion_tokens": null}},
        // Router models report -1: the price depends on the model they pick.
        {"id": "router", "pricing": {"prompt": "-1", "completion": "-1"}},
    ]);
    let server = serve(200, json!({"data": data})).await;
    let catalog = vec![
        chat("listed"),
        chat("no-limits"),
        chat("router"),
        chat("blocked"),
        typed("image", "image"),
        typed("classifier", "classifier"),
    ];
    let result = personalize_openrouter_catalog(catalog, &context(&server, Some("sk-or-test"))).await.unwrap();

    assert_eq!(ids(&result), ["listed", "no-limits", "router", "image", "classifier"]);
    assert_eq!((result[0].context_window, result[0].max_tokens), (262_144, 32_768));
    assert_eq!(result[0].cost, ModelCost { input: 3.0, output: 15.0, ..ModelCost::default() });
    assert_eq!((result[1].context_window, result[1].max_tokens), (200_000, 50_000));
    assert_eq!(result[1].cost, chat("x").cost);
    assert_eq!(result[2].cost, ModelCost::default());
    assert_eq!(result[3], typed("image", "image"));
    let request = &server.requests()[0];
    assert_eq!(request.path, "/api/v1/models/user");
    assert_eq!(request.headers["authorization"], "Bearer sk-or-test");
}

#[tokio::test]
async fn returns_the_catalog_unchanged_when_the_endpoint_rejects_the_key_or_is_missing() {
    for status in [401, 403, 404] {
        let server = serve(status, json!({"error": "no"})).await;
        let catalog = vec![chat("a")];
        let result = personalize_openrouter_catalog(catalog.clone(), &context(&server, Some("k"))).await;
        assert_eq!(result.unwrap(), catalog, "status {status}");
    }
}

#[tokio::test]
async fn fails_on_other_statuses_and_invalid_bodies_and_skips_without_a_key() {
    let server = serve(500, json!({})).await;
    let error = personalize_openrouter_catalog(vec![chat("a")], &context(&server, Some("k"))).await.unwrap_err();
    assert_eq!(error, "OpenRouter model list request failed: 500");
    let server = serve(200, json!({"models": []})).await;
    let error = personalize_openrouter_catalog(vec![chat("a")], &context(&server, Some("k"))).await.unwrap_err();
    assert_eq!(error, "Invalid OpenRouter model list response");

    let server = serve(200, json!({"data": []})).await;
    let result = personalize_openrouter_catalog(vec![chat("a")], &context(&server, None)).await.unwrap();
    assert_eq!(ids(&result), ["a"]);
    assert!(server.requests().is_empty(), "no request without a key");
}

#[test]
fn prompt_length_overrides_become_tiers_and_time_overrides_are_skipped() {
    let pricing = json!({"prompt": "0.000001", "completion": 0.000002, "overrides": [
        {"min_prompt_tokens": 200_000, "prompt": "0.000002"},
        {"min_prompt_tokens": 100_000, "utc_start": 0, "prompt": "0.000009"},
        {"prompt": "0.000009"},
    ]});
    let cost = openrouter_cost(Some(&pricing));
    assert_eq!((cost.input, cost.output), (1.0, 2.0));
    assert_eq!(cost.tiers.len(), 1);
    assert_eq!((cost.tiers[0].input_tokens_above, cost.tiers[0].input, cost.tiers[0].output), (200_000, 2.0, 2.0));
    assert_eq!(openrouter_cost(None), ModelCost::default());
}
