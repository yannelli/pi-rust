//! Model catalogs at startup: the bundled list with the `models-store.json` offline cache, then
//! the pi.dev overlay from a local catalog server (upstream `test/model-catalog-refresh.test.ts`).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{
    ApiKeyCredential, AuthError, AuthInteraction, AuthResult, Credential, CredentialStore, InMemoryCredentialStore,
    LoginOptions, ModelAuth, OAuthAuth, OAuthCredential,
};
use pi_coding_agent::agent_session::{CatalogSetup, ModelRuntime, ModelRuntimeOptions};
use pi_coding_agent::catalog::{CatalogConfig, FileModelsStore};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent_session::http_mock::{Reply, Server};

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
        credential_env: crate::offline_guard::credential_free_env(),
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
        "/v1/models" => Reply::json(&json!({"data": [{"id": "claude-opus-4-8"}, {"id": "claude-remote"}]})),
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
    let ids: Vec<&str> =
        stored["anthropic"]["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(ids, ["claude-opus-4-8", "claude-remote"], "the key's model list applies");
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
async fn disabled_providers_stay_hidden_after_refresh_and_can_be_reenabled() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(|_| Reply::status(404)).await;
    route_anthropic(dir.path(), &server.url);
    std::fs::write(dir.path().join("settings.json"), r#"{"disabledProviders":["anthropic","openrouter"]}"#).unwrap();
    // A project cannot undo the global exclusions.
    std::fs::create_dir(dir.path().join(".pi")).unwrap();
    std::fs::write(dir.path().join(".pi/settings.json"), r#"{"disabledProviders":[]}"#).unwrap();
    let cached = json!({"anthropic": {"models": [model_json("anthropic", "cached")], "checkedAt": 1,
        "piVersion": env!("CARGO_PKG_VERSION")}});
    std::fs::write(dir.path().join("models-store.json"), cached.to_string()).unwrap();
    let runtime = start(dir.path(), &server.url).await;
    for _ in 0..2 {
        assert!(
            runtime.all_models().iter().all(|model| !["anthropic", "openrouter"].contains(&model.provider.as_str()))
        );
        assert!(runtime.available_models().iter().all(|model| model.provider != "anthropic"));
        assert!(runtime.registry().models().iter().all(|model| model.provider != "anthropic"));
        assert!(runtime.get_model("anthropic", "cached").is_none());
        assert!(runtime.credential_store().read("anthropic").await.unwrap().is_some());
        runtime.refresh_catalogs(false, None, false).await;
    }
    runtime
        .register_extension_provider(
            "openrouter",
            json!({"name": "Router", "api": "openai-completions", "baseUrl": server.url,
        "apiKey": "test-key", "models": [{"id": "extension-model"}]}),
        )
        .unwrap();
    assert!(runtime.get_model("openrouter", "extension-model").is_none());
    let providers = runtime.all_providers();
    assert!(providers.contains(&("anthropic".into(), "anthropic".into())));
    assert!(providers.contains(&("openrouter".into(), "Router".into())));
    assert!(providers.windows(2).all(|pair| pair[0].0 < pair[1].0));
    runtime.set_disabled_providers(Vec::new());
    assert!(runtime.get_model("openrouter", "extension-model").is_some());
    assert!(runtime.get_model("anthropic", "cached").is_some());
    assert!(runtime.available_models().iter().any(|model| model.provider == "anthropic"));
    assert!(server.requests().is_empty());
}

/// A login that counts its token requests.
#[derive(Default)]
struct CountingOAuth {
    requests: AtomicUsize,
}

#[async_trait]
impl OAuthAuth for CountingOAuth {
    fn name(&self) -> &str {
        "Counting"
    }

    async fn login(
        &self,
        _interaction: &dyn AuthInteraction,
        _options: &LoginOptions,
        _cancel: CancellationToken,
    ) -> AuthResult<OAuthCredential> {
        Err(AuthError::other("unused"))
    }

    async fn refresh(&self, _credential: &OAuthCredential, _cancel: CancellationToken) -> AuthResult<OAuthCredential> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Err(AuthError::other("unexpected token request"))
    }

    fn to_auth(&self, credential: &OAuthCredential) -> ModelAuth {
        ModelAuth { api_key: Some(credential.access.clone()), ..ModelAuth::default() }
    }
}

// Upstream #11692: a client may stop pi right after startup, before a rotated single-use refresh
// token is saved, so a catalog refresh uses a valid login and leaves an expiring one alone.
#[tokio::test]
async fn catalog_refreshes_do_not_rotate_an_expiring_login() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(|request| match request.path() {
        "/v1/models" => Reply::json(&json!({"data": [{"id": "claude-opus-4-8"}]})),
        _ => Reply::status(404),
    })
    .await;
    route_anthropic(dir.path(), &server.url);
    let login = |expires| Credential::OAuth(OAuthCredential { access: "token".into(), expires, ..Default::default() });
    let store = Arc::new(InMemoryCredentialStore::new(BTreeMap::from([("anthropic".to_string(), login(0))])));
    let oauth = Arc::new(CountingOAuth::default());
    let lookup = oauth.clone();
    let options = ModelRuntimeOptions {
        models_path: dir.path().join("models.json"),
        credentials: store.clone(),
        oauth: Some(Arc::new(move |id: &str| (id == "anthropic").then(|| lookup.clone() as Arc<dyn OAuthAuth>))),
        credential_env: crate::offline_guard::credential_free_env(),
    };
    let runtime = ModelRuntime::create(options).await;
    let setup = CatalogSetup {
        store: Arc::new(FileModelsStore::new(dir.path().join("models-store.json"))),
        config: CatalogConfig { catalog_base_url: Some(server.url.clone()), ..CatalogConfig::default() },
    };
    runtime.enable_dynamic_catalogs(setup).await;

    let result = runtime.refresh_catalogs(true, Some(vec!["anthropic".into()]), true).await;
    assert!(result.errors.is_empty(), "{result:?}");
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 0);
    assert_eq!(server.count("/v1/models"), 0, "the expiring login is not used");
    assert_eq!(store.read("anthropic").await.unwrap(), Some(login(0)));

    store.update("anthropic", Box::new(move |_| Box::pin(async move { Ok(Some(login(i64::MAX))) }))).await.unwrap();
    let result = runtime.refresh_catalogs(true, Some(vec!["anthropic".into()]), true).await;
    assert!(result.errors.is_empty(), "{result:?}");
    assert_eq!(server.count("/v1/models"), 1, "a valid login lists the account's models");
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 0);
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
    assert!(runtime.all_providers().contains(&("local".into(), "local".into())));
    assert!(runtime.all_models().len() > 100, "the bundled catalog stays");
}
