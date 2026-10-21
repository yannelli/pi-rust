//! Port of upstream's personalized catalog provider tests, plus the OpenRouter wiring and the
//! refresh interval, unmatched lists, and version check of pi-rust's account model lists.

use std::sync::{Arc, Mutex};

use pi_ai::Model;
use pi_ai::auth::{ApiKeyCredential, Credential};
use pi_ai::model_lists::{ModelListContext, Personalized};
use serde_json::json;

use super::{RecordingPublisher, ids, model, model_json, refresh_with};
use crate::catalog::*;
use crate::platform::http::VERSION;
use crate::platform::test_http::{TestReply, TestServer, client};

const PROVIDER: &str = "test-provider";

fn key(key: &str) -> Credential {
    Credential::ApiKey(ApiKeyCredential { key: Some(key.into()), env: None })
}

/// A personalize step that records its input and key, and keeps the models in `allowed`.
fn keep(allowed: &'static [&'static str], seen: &Arc<Mutex<Vec<String>>>) -> Personalize {
    let seen = seen.clone();
    Arc::new(move |catalog: Vec<Model>, context: ModelListContext| {
        let mut log = seen.lock().unwrap();
        log.push(format!("key={}", context.key.unwrap_or_default()));
        log.extend(catalog.iter().map(|model| model.id.clone()));
        let kept = catalog.into_iter().filter(|model| allowed.contains(&model.id.as_str())).collect();
        Box::pin(async move { Ok(Personalized::Models(kept)) })
    })
}

/// A list client like pi's that also ignores proxy variables, for the local server.
fn list_client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
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
        list_client: Some(list_client()),
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

#[tokio::test]
async fn an_unmatched_list_shows_the_catalog_and_keeps_the_saved_list() {
    let server = TestServer::start(|_| TestReply::status(404, "")).await;
    let defaults = Arc::new(Mutex::new(Vec::new()));
    let seen = defaults.clone();
    let unmatched: Personalize = Arc::new(move |catalog, context: ModelListContext| {
        seen.lock().unwrap().push(context.default_model);
        Box::pin(async move { Ok(Personalized::Unmatched(catalog)) })
    });
    let builtin = vec![model("anthropic", "claude-opus-4-8"), model("anthropic", "claude-haiku-4-5")];
    let provider = PersonalizedCatalog::new("anthropic", builtin, unmatched)
        .with_catalog(Some(&server.url), None)
        .with_client(client())
        .with_interval(Some(REMOTE_CATALOG_REFRESH_INTERVAL_MS));
    let saved = ModelsStoreEntry {
        pi_version: Some(VERSION.into()),
        ..ModelsStoreEntry::with_models(vec![model("anthropic", "claude-haiku-4-5")])
    };
    let publisher = RecordingPublisher::new(Some(saved.clone()));

    refresh_with(&provider, &publisher, Some(&key("k")), true, true).await.unwrap();
    assert_eq!(ids(&provider.models()), ["claude-opus-4-8", "claude-haiku-4-5"]);
    assert_eq!(publisher.stored(), Some(saved), "nothing is saved");
    let expected = crate::model_resolver::default_model_for("anthropic").map(String::from);
    assert_eq!(*defaults.lock().unwrap(), [expected], "the list learns the provider's default model");
}

#[tokio::test]
async fn a_list_saved_by_another_version_waits_for_a_refresh() {
    let server = TestServer::start(|_| TestReply::status(404, "")).await;
    let seen = Arc::default();
    let builtin = vec![model(PROVIDER, "static"), model(PROVIDER, "new")];
    let provider = PersonalizedCatalog::new(PROVIDER, builtin, keep(&["static"], &seen))
        .with_catalog(Some(&server.url), None)
        .with_client(client())
        .with_interval(Some(REMOTE_CATALOG_REFRESH_INTERVAL_MS));
    let models = vec![model(PROVIDER, "static")];
    for version in [Some("0.0.1-old".to_string()), None] {
        let entry = ModelsStoreEntry {
            checked_at: Some(pi_ai::now_ms()),
            pi_version: version,
            ..ModelsStoreEntry::with_models(models.clone())
        };
        let publisher = RecordingPublisher::new(Some(entry));
        refresh_with(&provider, &publisher, None, false, false).await.unwrap();
        assert_eq!(ids(&provider.models()), ["static", "new"], "offline, the full catalog until a refresh");
        refresh_with(&provider, &publisher, Some(&key("k")), true, false).await.unwrap();
        assert_eq!(ids(&provider.models()), ["static"], "a recent list of another version is refreshed");
        assert_eq!(publisher.stored().unwrap().pi_version.as_deref(), Some(VERSION));
    }
    assert_eq!(server.requests().len(), 2);
}
