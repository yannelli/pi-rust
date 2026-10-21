//! Port of upstream's personalized catalog provider tests, plus the OpenRouter wiring and the
//! refresh interval of pi-rust's account model lists.

use std::sync::{Arc, Mutex};

use pi_ai::Model;
use pi_ai::auth::{ApiKeyCredential, Credential};
use serde_json::json;

use super::{RecordingPublisher, ids, model, model_json, refresh_with};
use crate::catalog::*;
use crate::platform::test_http::{TestReply, TestServer, client};

const PROVIDER: &str = "test-provider";

fn key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
}

/// A personalize step that records its input and key, and keeps the models in `allowed`.
fn keep(allowed: &'static [&'static str], seen: &Arc<Mutex<Vec<String>>>) -> Personalize {
    let seen = seen.clone();
    Arc::new(move |catalog: Vec<Model>, context: pi_ai::model_lists::ModelListContext| {
        let mut log = seen.lock().unwrap();
        log.push(format!("key={}", context.key.unwrap_or_default()));
        log.extend(catalog.iter().map(|model| model.id.clone()));
        Box::pin(async move { Ok(catalog.into_iter().filter(|model| allowed.contains(&model.id.as_str())).collect()) })
    })
}

fn failing() -> Personalize {
    Arc::new(|_, _| Box::pin(async { Err("user models request failed".to_string()) }))
}

fn catalog(server: &TestServer, personalize: Personalize) -> PersonalizedCatalog {
    PersonalizedCatalog::new(PROVIDER, vec![model(PROVIDER, "static")], personalize)
        .with_catalog(Some(&server.url), None)
        .with_client(client())
}

fn image(id: &str) -> Model {
    let mut image = model(PROVIDER, id);
    image.model_type = Some("image".into());
    image
}

#[tokio::test]
async fn keeps_the_saved_list_and_saves_nothing_when_personalization_fails() {
    // Saving the catalog without personalization would replace the key's list with every model.
    let server = TestServer::start(|_| TestReply::status(404, "not found")).await;
    let saved =
        ModelsStoreEntry { checked_at: Some(1), ..ModelsStoreEntry::with_models(vec![model(PROVIDER, "allowed")]) };
    let publisher = RecordingPublisher::new(Some(saved.clone()));
    let provider = catalog(&server, failing());

    let error = refresh_with(&provider, &publisher, Some(&key("k")), true, false).await.unwrap_err();
    assert_eq!(error.to_string(), "user models request failed");
    assert_eq!(ids(&provider.models()), ["allowed"]);
    assert_eq!(publisher.stored(), Some(saved));
}

#[tokio::test]
async fn restores_a_saved_list_as_is_and_falls_back_to_the_bundled_models_only_when_empty() {
    let server = TestServer::start(|_| TestReply::status(500, "unused")).await;
    let provider = catalog(&server, failing());
    // Older versions saved an empty list after a missing pi.dev catalog; only such an entry is empty.
    let publisher = RecordingPublisher::new(Some(ModelsStoreEntry { checked_at: Some(1), ..Default::default() }));
    refresh_with(&provider, &publisher, None, false, false).await.unwrap();
    assert_eq!(ids(&provider.compose(vec![model(PROVIDER, "static")])), ["static"]);

    // A key that may use no chat models keeps its image models and shows no chat models.
    let publisher = RecordingPublisher::new(Some(ModelsStoreEntry::with_models(vec![image("flux")])));
    refresh_with(&provider, &publisher, None, false, false).await.unwrap();
    assert_eq!(ids(&provider.compose(vec![model(PROVIDER, "static")])), ["flux"]);
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn personalizes_the_bundled_and_newer_pi_dev_catalog_and_saves_the_result() {
    let server = TestServer::start(|_| TestReply::json(&json!([model_json(PROVIDER, "remote")]))).await;
    let seen = Arc::default();
    let provider = catalog(&server, keep(&["remote"], &seen));
    let publisher = RecordingPublisher::new(None);
    refresh_with(&provider, &publisher, Some(&key("sk-test")), true, false).await.unwrap();

    assert_eq!(*seen.lock().unwrap(), ["key=sk-test", "static", "remote"]);
    assert_eq!(ids(&provider.compose(vec![model(PROVIDER, "static")])), ["remote"]);
    let stored = publisher.stored().unwrap();
    assert_eq!(ids(&stored.models), ["remote"]);
    assert!(stored.checked_at.is_some() && stored.last_modified.is_none() && stored.etag.is_none());
    assert_eq!(server.requests()[0].header("if-none-match"), None, "no revalidation");

    // Upstream lists refresh on every network refresh.
    refresh_with(&provider, &publisher, Some(&key("sk-test")), true, false).await.unwrap();
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn pi_rust_lists_wait_for_the_interval_unless_forced() {
    let server = TestServer::start(|_| TestReply::status(404, "")).await;
    let seen = Arc::default();
    let provider = catalog(&server, keep(&["static"], &seen)).with_interval(Some(REMOTE_CATALOG_REFRESH_INTERVAL_MS));
    // An entry with pi.dev validators came from the plain overlay, so it is refreshed at once.
    let overlay_entry =
        ModelsStoreEntry { checked_at: Some(pi_ai::now_ms()), last_modified: Some(1), ..ModelsStoreEntry::default() };
    let publisher =
        RecordingPublisher::new(Some(ModelsStoreEntry { models: vec![model(PROVIDER, "old")], ..overlay_entry }));
    refresh_with(&provider, &publisher, None, false, false).await.unwrap();
    assert_eq!(ids(&provider.models()), ["static", "old"], "read as an overlay of the bundled models");
    refresh_with(&provider, &publisher, Some(&key("k")), true, false).await.unwrap();
    assert_eq!(server.requests().len(), 1);
    refresh_with(&provider, &publisher, Some(&key("k")), true, false).await.unwrap();
    assert_eq!(server.requests().len(), 1, "a list saved within the interval is kept");
    refresh_with(&provider, &publisher, Some(&key("k")), true, true).await.unwrap();
    assert_eq!(server.requests().len(), 2, "forced refreshes go online");
    assert_eq!(ids(&provider.models()), ["static"]);
}

#[tokio::test]
async fn openrouter_lists_the_keys_models_from_the_configured_base_url() {
    let server = TestServer::start(|request| match request.path() {
        "/api/v1/models/user" => TestReply::json(&json!({"data": [{"id": "allowed", "context_length": 64_000}]})),
        _ => TestReply::status(404, ""),
    })
    .await;
    let builtin = vec![model("openrouter", "allowed"), model("openrouter", "blocked"), model("openai", "gpt")];
    let models_json = json!({"providers": {"openrouter": {"baseUrl": format!("{}/api/v1", server.url)}}});
    let config = CatalogConfig {
        catalog_base_url: Some(server.url.clone()),
        models_json: Some(models_json.to_string()),
        client: Some(client()),
        ..CatalogConfig::default()
    };
    let catalogs = builtin_catalogs(&builtin, &config);
    let openrouter = catalogs.iter().find(|catalog| catalog.provider_id() == "openrouter").unwrap();
    let publisher = RecordingPublisher::new(None);
    refresh_with(openrouter.as_ref(), &publisher, Some(&key("sk-or-test")), true, false).await.unwrap();

    let composed = compose_catalog(&builtin, &catalogs);
    assert_eq!(ids(&composed), ["allowed", "gpt"]);
    assert_eq!(composed[0].context_window, 64_000);
    let request = server.requests().into_iter().find(|request| request.path() == "/api/v1/models/user").unwrap();
    assert_eq!(request.header("authorization"), Some("Bearer sk-or-test"));
    let saved = serde_json::to_string(&publisher.stored().unwrap()).unwrap();
    assert!(!saved.contains("sk-or-test"), "the key is never saved");
}
