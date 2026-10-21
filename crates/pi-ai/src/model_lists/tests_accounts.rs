//! pi-rust's account model lists against local servers: filter, enrich, add, and fallbacks.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use super::reconcile::{AddRule, Listed, reconcile};
use super::tests_openrouter::serve;
use super::*;
use crate::providers::http_mock::{Reply, RouteServer};

pub(super) fn chat(provider: &str, api: &str, id: &str) -> Model {
    serde_json::from_value(json!({"id": id, "name": id, "api": api, "provider": provider,
        "baseUrl": "https://example.test/v1", "reasoning": true, "input": ["text"], "contextWindow": 100_000,
        "maxTokens": 10_000, "headers": {"x-static": "1"}, "compat": {"flag": id},
        "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0}}))
    .unwrap()
}

pub(super) fn classifier(provider: &str, id: &str) -> Model {
    Model { model_type: Some("classifier".into()), ..chat(provider, "openai-decisions", id) }
}

pub(super) fn ids(models: &[Model]) -> Vec<&str> {
    models.iter().map(|model| model.id.as_str()).collect()
}

pub(super) fn context(server: &RouteServer, key: Option<&str>) -> ModelListContext {
    ModelListContext {
        base_url: Some(server.base_url.clone()),
        client: reqwest::Client::builder().no_proxy().build().unwrap(),
        ..ModelListContext::new(key.map(String::from))
    }
}

/// Every rejecting status and an unreadable body leave `catalog` unchanged; other failing
/// statuses are errors.
pub(super) async fn assert_fallbacks(list: AccountModelList, key: &str, catalog: Vec<Model>) {
    for status in [401, 403, 404] {
        let server = serve(status, json!({"error": "no"})).await;
        let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
        assert_eq!(result.unwrap(), catalog, "{list:?} status {status}");
    }
    let server = RouteServer::start(|_| Reply::new(200, "application/json", "{ not json")).await;
    let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
    assert_eq!(result.unwrap(), catalog, "{list:?} bad JSON");
    let server = serve(200, json!({"unexpected": true})).await;
    let result = list.personalize(catalog.clone(), &context(&server, Some(key))).await;
    assert_eq!(result.unwrap(), catalog, "{list:?} unknown shape");
    let server = serve(502, json!({})).await;
    assert!(list.personalize(catalog.clone(), &context(&server, Some(key))).await.is_err(), "{list:?} 502");
    let server = serve(200, json!({})).await;
    let result = list.personalize(catalog.clone(), &context(&server, None)).await;
    assert_eq!(result.unwrap(), catalog);
    assert!(server.requests().is_empty(), "{list:?} sends nothing without a key");
}

#[test]
fn reconcile_keeps_aliases_takes_limits_and_adds_copies_of_the_closest_model() {
    let catalog = vec![
        chat("p", "a", "claude-opus-4-5"),
        chat("p", "a", "claude-opus-4-5-20251101"),
        chat("p", "b", "claude-sonnet-4-6"),
        chat("p", "a", "claude-haiku-4-5"),
        classifier("p", "judge"),
    ];
    let listed = [
        Listed { context_window: Some(500_000), ..Listed::new("claude-opus-4-5-20251101") },
        Listed { name: Some("Sonnet 4.7".into()), max_tokens: Some(64_000), ..Listed::new("claude-sonnet-4-7") },
        Listed::new("claude-sonnet-4-7-20261001"),
    ];
    let models = reconcile(catalog, &listed, AddRule::Any);

    assert_eq!(ids(&models), ["claude-opus-4-5", "claude-opus-4-5-20251101", "judge", "claude-sonnet-4-7"]);
    assert_eq!(models[0].context_window, 500_000, "an alias takes its snapshot's limits");
    let added = &models[3];
    assert_eq!((added.name.as_str(), added.api.as_str(), added.max_tokens), ("Sonnet 4.7", "b", 64_000));
    assert_eq!((added.context_window, added.base_url.as_str()), (100_000, "https://example.test/v1"));
    assert_eq!(added.compat, Some(json!({"flag": "claude-sonnet-4-6"})));
    assert_eq!(added.headers["x-static"], "1");
    assert_eq!(reconcile(vec![chat("p", "a", "x")], &[], AddRule::Any), vec![chat("p", "a", "x")]);
}

#[test]
fn catalog_families_add_only_chat_models_of_known_families() {
    let catalog = vec![chat("openai", "r", "gpt-5.4"), chat("openai", "r", "o3")];
    let listed: Vec<Listed> = [
        "gpt-5.4",
        "gpt-5.9",
        "o3-deep-research",
        "gpt-image-2",
        "text-embedding-3-large",
        "gpt-4o-mini-tts",
        "omni-moderation-latest",
        "gpt-realtime",
        "davinci-002",
        "ft:gpt-4o:org::x",
        "whisper-1",
    ]
    .into_iter()
    .map(Listed::new)
    .collect();
    let models = reconcile(catalog, &listed, AddRule::CatalogFamilies);
    assert_eq!(ids(&models), ["gpt-5.4", "gpt-5.9", "o3-deep-research"]);
    assert_eq!(models[1].compat, Some(json!({"flag": "gpt-5.4"})));
}

fn codex_token(account: Option<&str>) -> String {
    let claims = match account {
        Some(account) => json!({"https://api.openai.com/auth": {"chatgpt_account_id": account}}),
        None => json!({"sub": "user"}),
    };
    format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
}

fn codex_catalog() -> Vec<Model> {
    let codex = |id| chat("openai-codex", "openai-codex-responses", id);
    vec![codex("gpt-5.5"), codex("gpt-6-sol"), codex("gpt-6.1-sol"), codex("gpt-5.3-codex-spark")]
}

#[tokio::test]
async fn codex_lists_the_accounts_visible_models() {
    let body = json!({"models": [
        {"slug": "gpt-6-sol", "display_name": "GPT-6 Sol", "visibility": "list", "supported_in_api": true,
         "context_window": 400_000, "supported_reasoning_levels": [{"effort": "low"}], "extra": {"x": 1}},
        {"slug": "gpt-6.1-sol", "visibility": "hide"},
        {"id": "gpt-6.2-sol", "display_name": "GPT-6.2 Sol"},
        {"slug": "gpt-5.5", "visibility": "LIST"},
        {"visibility": "list"},
    ]});
    let server = serve(200, body).await;
    let token = codex_token(Some("acct-test"));
    let models =
        AccountModelList::OpenAiCodex.personalize(codex_catalog(), &context(&server, Some(&token))).await.unwrap();

    assert_eq!(ids(&models), ["gpt-5.5", "gpt-6-sol", "gpt-6.2-sol"]);
    assert_eq!((models[1].context_window, models[1].name.as_str()), (400_000, "gpt-6-sol"));
    assert_eq!((models[2].name.as_str(), models[2].api.as_str()), ("GPT-6.2 Sol", "openai-codex-responses"));
    assert_eq!(models[2].compat, Some(json!({"flag": "gpt-6.1-sol"})), "copied from the closest model");
    let request = &server.requests()[0];
    let version = env!("CARGO_PKG_VERSION");
    assert_eq!(request.path, format!("/codex/models?client_version={version}"));
    assert_eq!(request.headers["authorization"], format!("Bearer {token}"));
    assert_eq!(
        (request.headers["chatgpt-account-id"].as_str(), request.headers["originator"].as_str()),
        ("acct-test", "pi")
    );
}

#[tokio::test]
async fn codex_falls_back_to_the_catalog() {
    assert_fallbacks(AccountModelList::OpenAiCodex, &codex_token(Some("acct-test")), codex_catalog()).await;
    let server = serve(200, json!({"models": []})).await;
    let token = codex_token(None);
    let models =
        AccountModelList::OpenAiCodex.personalize(codex_catalog(), &context(&server, Some(&token))).await.unwrap();
    assert_eq!(models, codex_catalog());
    assert!(server.requests().is_empty(), "a token without a ChatGPT account is not sent");
}
