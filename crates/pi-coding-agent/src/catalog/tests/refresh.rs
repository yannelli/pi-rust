//! The refresh orchestration: cache restore, credential gating, errors, cancellation, and
//! superseded refreshes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_ai::auth::{ApiKeyCredential, Credential};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ids, model, model_json};
use crate::catalog::*;
use crate::platform::test_http::{TestReply, TestServer, client};

/// Configured providers get an empty API key credential; others none.
struct FakeCredentials {
    configured: Vec<&'static str>,
    stored_error: Option<&'static str>,
}

#[async_trait]
impl RefreshCredentials for FakeCredentials {
    async fn stored(&self, _provider_id: &str) -> anyhow::Result<Option<Credential>> {
        match self.stored_error {
            Some(error) => anyhow::bail!(error),
            None => Ok(None),
        }
    }

    async fn resolve(&self, provider_id: &str, _stored: Option<&Credential>) -> anyhow::Result<Option<Credential>> {
        Ok(self.configured.contains(&provider_id).then(|| Credential::ApiKey(ApiKeyCredential::default())))
    }
}

fn refresher(store: Arc<InMemoryModelsStore>, configured: Vec<&'static str>) -> CatalogRefresher {
    CatalogRefresher::new(store, Arc::new(FakeCredentials { configured, stored_error: None }))
}

fn overlay(server: &TestServer, id: &str) -> Arc<RemoteCatalogOverlay> {
    Arc::new(RemoteCatalogOverlay::new(id, Some(&server.url), None).with_client(client()))
}

#[tokio::test]
async fn only_configured_providers_go_online_and_errors_are_collected() {
    let server = TestServer::start(|request| match request.path() {
        "/api/models/providers/a" => TestReply::json(&json!([model_json("a", "a-remote")])),
        _ => TestReply::status(500, "boom"),
    })
    .await;
    let store = Arc::new(InMemoryModelsStore::new());
    let (a, b, c) = (overlay(&server, "a"), overlay(&server, "b"), overlay(&server, "c"));
    let catalogs: Vec<Arc<dyn DynamicCatalog>> = vec![a.clone(), b.clone(), c.clone()];
    let result = refresher(store.clone(), vec!["a", "b"]).refresh(&catalogs, &RefreshOptions::default()).await;
    assert!(!result.aborted);
    assert_eq!(result.errors.keys().collect::<Vec<_>>(), ["b"]);
    assert_eq!(result.errors["b"], "Model catalog request failed for b: 500");
    assert_eq!(ids(&a.overlay_models()), ["a-remote"]);
    assert_eq!(server.count("/api/models/providers/c"), 0);
    let cancel = CancellationToken::new();
    assert_eq!(ids(&store.read("a", &cancel).await.unwrap().unwrap().models), ["a-remote"]);
    assert!(store.read("b", &cancel).await.unwrap().unwrap().models.is_empty());

    let selected = RefreshOptions { providers: Some(vec!["c".into()]), ..RefreshOptions::default() };
    let result = refresher(store.clone(), vec!["c"]).refresh(&catalogs, &selected).await;
    assert_eq!(result.errors.keys().collect::<Vec<_>>(), ["c"]);
    assert_eq!(server.count("/api/models/providers/a"), 1);
}

#[tokio::test]
async fn offline_refresh_restores_from_the_store_without_requests() {
    let server = TestServer::start(|_| TestReply::status(500, "")).await;
    let store = Arc::new(InMemoryModelsStore::new());
    let cancel = CancellationToken::new();
    store.write("a", &ModelsStoreEntry::with_models(vec![model("a", "cached")]), &cancel).await.unwrap();
    let a = overlay(&server, "a");
    let catalogs: Vec<Arc<dyn DynamicCatalog>> = vec![a.clone()];
    let options = RefreshOptions { allow_network: false, ..RefreshOptions::default() };
    let result = refresher(store, vec!["a"]).refresh(&catalogs, &options).await;
    assert_eq!(result, RefreshResult::default());
    assert_eq!(ids(&a.overlay_models()), ["cached"]);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn credential_read_errors_still_restore_the_cache() {
    let store = Arc::new(InMemoryModelsStore::new());
    let cancel = CancellationToken::new();
    store.write("a", &ModelsStoreEntry::with_models(vec![model("a", "cached")]), &cancel).await.unwrap();
    let a = Arc::new(RemoteCatalogOverlay::new("a", Some("http://127.0.0.1:9"), None));
    let catalogs: Vec<Arc<dyn DynamicCatalog>> = vec![a.clone()];
    let credentials = FakeCredentials { configured: vec!["a"], stored_error: Some("auth.json is invalid") };
    let result =
        CatalogRefresher::new(store, Arc::new(credentials)).refresh(&catalogs, &RefreshOptions::default()).await;
    assert_eq!(result.errors["a"], "auth.json is invalid");
    assert_eq!(ids(&a.overlay_models()), ["cached"]);
}

#[tokio::test]
async fn cancellation_aborts_without_errors() {
    let (sender, receiver) = mpsc::unbounded_channel::<Vec<u8>>();
    let receiver = Mutex::new(Some(receiver));
    let server = TestServer::start(move |_| TestReply::Stream(receiver.lock().unwrap().take().unwrap())).await;
    let catalogs: Vec<Arc<dyn DynamicCatalog>> = vec![overlay(&server, "a")];
    let options = RefreshOptions::default();
    let cancel = options.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let result = refresher(Arc::new(InMemoryModelsStore::new()), vec!["a"]).refresh(&catalogs, &options).await;
    assert!(result.aborted);
    assert!(result.errors.is_empty());
    drop(sender);
}

#[tokio::test]
async fn a_newer_refresh_wins_over_a_stalled_older_one() {
    let (sender, receiver) = mpsc::unbounded_channel::<Vec<u8>>();
    let receiver = Mutex::new(Some(receiver));
    let server = TestServer::start(move |_| match receiver.lock().unwrap().take() {
        Some(stalled) => TestReply::Stream(stalled),
        None => TestReply::json(&json!([model_json("a", "newer")])),
    })
    .await;
    let store = Arc::new(InMemoryModelsStore::new());
    let refresher = Arc::new(refresher(store.clone(), vec!["a"]));
    let a = overlay(&server, "a");
    let catalogs: Vec<Arc<dyn DynamicCatalog>> = vec![a.clone()];
    let force = RefreshOptions { force: true, ..RefreshOptions::default() };
    let older = {
        let (refresher, catalogs, force) = (refresher.clone(), catalogs.clone(), force.clone());
        tokio::spawn(async move { refresher.refresh(&catalogs, &force).await })
    };
    while server.requests().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let newer = refresher.refresh(&catalogs, &force).await;
    assert_eq!(newer, RefreshResult::default());
    assert_eq!(ids(&a.overlay_models()), ["newer"]);

    sender.send(json!([model_json("a", "stale")]).to_string().into_bytes()).unwrap();
    drop(sender);
    let older = older.await.unwrap();
    assert!(older.errors.is_empty(), "{older:?}");
    assert_eq!(ids(&a.overlay_models()), ["newer"]);
    let stored = store.read("a", &CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(ids(&stored.models), ["newer"]);
}

#[tokio::test]
async fn file_store_backs_update_models() {
    let server = TestServer::start(|_| TestReply::json(&json!({}))).await;
    let dir = tempfile::tempdir().unwrap();
    let config = CatalogConfig {
        catalog_base_url: Some(server.url.clone()),
        client: Some(client()),
        models_json: Some("{}".into()),
        ..CatalogConfig::default()
    };
    assert_eq!(refresh_model_catalogs(dir.path(), &config).await.unwrap(), "Model catalogs refreshed");
}
