//! Model catalogs at startup: the bundled list with the `models-store.json` offline cache, then
//! the pi.dev overlay from a local catalog server (upstream `test/model-catalog-refresh.test.ts`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pi_ai::auth::{ApiKeyCredential, Credential, InMemoryCredentialStore};
use pi_coding_agent::agent_session::{CatalogSetup, ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::catalog::{CatalogConfig, FileModelsStore};
use serde_json::{Value, json};

use crate::http_mock::{Reply, Server};

fn model_json(provider: &str, id: &str) -> Value {
    json!({"id": id, "name": id, "api": "anthropic-messages", "provider": provider,
        "baseUrl": "https://api.anthropic.com", "contextWindow": 200000, "maxTokens": 8192})
}

/// Send Anthropic's model list to the local server too, so no test reaches the real API.
fn route_anthropic(dir: &std::path::Path, url: &str) {
    let models = json!({"providers": {"anthropic": {"baseUrl": url}}});
    std::fs::write(dir.join("models.json"), models.to_string()).unwrap();
}

async fn start(dir: &std::path::Path, catalog_url: &str) -> Arc<ModelRuntime> {
    let anthropic = Credential::ApiKey(ApiKeyCredential { key: Some("sk-test".into()), env: None });
    let credentials = BTreeMap::from([("anthropic".to_string(), anthropic)]);
    let options = ModelRuntimeOptions {
        models_path: dir.join("models.json"),
        credentials: Arc::new(InMemoryCredentialStore::new(credentials)),
        oauth: None,
    };
    let runtime = ModelRuntime::create(options).await;
    let setup = CatalogSetup {
        store: Arc::new(FileModelsStore::new(dir.join("models-store.json"))),
        config: CatalogConfig { catalog_base_url: Some(catalog_url.into()), ..CatalogConfig::default() },
    };
    runtime.enable_dynamic_catalogs(setup).await;
    Arc::new(runtime)
}

#[tokio::test]
async fn startup_restores_the_offline_cache_and_refreshes_from_the_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let cached = json!({"openai": {"models": [model_json("openai", "gpt-cached")], "checkedAt": 1, "lastModified": 1}});
    std::fs::write(dir.path().join("models-store.json"), cached.to_string()).unwrap();
    let server = Server::start(|request| match request.path() {
        "/api/models/providers/anthropic" => Reply::json(&json!([model_json("anthropic", "claude-remote")])),
        "/v1/models" => Reply::json(&json!({"data": [{"id": "claude-remote"}], "has_more": false})),
        _ => Reply::status(404),
    })
    .await;
    route_anthropic(dir.path(), &server.url);

    let runtime = start(dir.path(), &server.url).await;
    assert!(runtime.has_dynamic_catalogs());
    assert!(runtime.get_model("openai", "gpt-cached").is_some(), "the offline cache is restored");
    assert!(server.requests().is_empty(), "startup does not go online");
    assert!(runtime.get_model("anthropic", "claude-remote").is_none());

    let result = runtime.refresh_catalogs(true, Some(vec!["anthropic".into(), "openai".into()]), false).await;
    assert!(result.errors.is_empty(), "{result:?}");
    let model = runtime.get_model("anthropic", "claude-remote").unwrap();
    assert!(runtime.available_models().contains(&model), "the provider has a key");
    assert_eq!(server.count("/api/models/providers/anthropic"), 1);
    assert_eq!(server.count("/api/models/providers/openai"), 0, "providers without credentials stay offline");
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("models-store.json")).unwrap()).unwrap();
    assert_eq!(stored["anthropic"]["models"][0]["id"], "claude-remote", "the key's model list applies");
    assert_eq!(stored["anthropic"]["models"].as_array().unwrap().len(), 1);
    assert!(!stored.to_string().contains("sk-test"), "the key is never saved");
    assert_eq!(stored["openai"]["models"][0]["id"], "gpt-cached");

    // A new runtime starts from the refreshed cache without network access.
    let next = start(dir.path(), &server.url).await;
    assert!(next.get_model("anthropic", "claude-remote").is_some());
    assert_eq!(server.count("/api/models/providers/anthropic"), 1);
}

#[tokio::test]
async fn background_refreshes_update_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(|request| match request.path() {
        "/api/models/providers/anthropic" => Reply::json(&json!({"models": [model_json("anthropic", "claude-bg")]})),
        _ => Reply::status(404),
    })
    .await;
    route_anthropic(dir.path(), &server.url);
    let runtime = start(dir.path(), &server.url).await;
    let Some(task) = runtime.refresh_in_background_within(Duration::from_secs(10)) else {
        // PI_OFFLINE is set in this environment: background refreshes are off.
        assert!(pi_coding_agent::config::offline());
        return;
    };
    let result = task.await.unwrap();
    assert!(!result.aborted);
    assert!(runtime.get_model("anthropic", "claude-bg").is_some());
}

#[tokio::test]
async fn models_json_still_applies_over_dynamic_catalogs() {
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {"local": {"baseUrl": "http://localhost:1", "api": "openai-completions",
        "apiKey": "key", "models": [{"id": "mine"}]}}});
    std::fs::write(dir.path().join("models.json"), models.to_string()).unwrap();
    let server = Server::start(|_| Reply::status(404)).await;
    let runtime = start(dir.path(), &server.url).await;
    assert!(runtime.get_model("local", "mine").is_some());
    assert!(runtime.has_configured_auth("local"));
    assert!(runtime.all_models().len() > 100, "the bundled catalog stays");
}
