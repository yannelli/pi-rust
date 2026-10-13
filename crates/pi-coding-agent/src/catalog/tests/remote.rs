//! Port of upstream `remote-catalog-provider.test.ts`, plus the Radius gateway catalog.

use std::sync::Arc;
use std::sync::Mutex;

use pi_ai::auth::{ApiKeyCredential, Credential, OAuthCredential};
use serde_json::json;

use super::{RecordingPublisher, ids, model, model_json, refresh_with};
use crate::catalog::remote::parse_http_date;
use crate::catalog::*;
use crate::platform::VERSION;
use crate::platform::test_http::{TestReply, TestServer, client};

fn credential() -> Credential {
    Credential::ApiKey(ApiKeyCredential::default())
}

fn overlay(server: &TestServer, local_generated_at: Option<i64>) -> RemoteCatalogOverlay {
    RemoteCatalogOverlay::new("test-provider", Some(&server.url), local_generated_at).with_client(client())
}

/// A server answering with `replies` in order, then 500s.
async fn scripted(replies: Vec<TestReply>) -> TestServer {
    let replies = Mutex::new(replies.into_iter());
    TestServer::start(move |_| replies.lock().unwrap().next().unwrap_or_else(|| TestReply::status(500, "done"))).await
}

async fn refresh(overlay: &RemoteCatalogOverlay, publisher: &RecordingPublisher, force: bool) -> anyhow::Result<()> {
    refresh_with(overlay, publisher, Some(&credential()), true, force).await
}

fn composed(overlay: &RemoteCatalogOverlay) -> Vec<String> {
    overlay.compose(vec![model("test-provider", "static")]).into_iter().map(|model| model.id).collect()
}

#[tokio::test]
async fn parses_keyed_catalogs_sends_headers_observes_ttl_and_forces() {
    let server =
        TestServer::start(|_| TestReply::json(&json!({"dynamic": model_json("test-provider", "dynamic")}))).await;
    let overlay = overlay(&server, None);
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    refresh(&overlay, &publisher, false).await.unwrap();
    refresh(&overlay, &publisher, true).await.unwrap();

    assert_eq!(composed(&overlay), ["static", "dynamic"]);
    assert_eq!(ids(&publisher.stored().unwrap().models), ["dynamic"]);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].header("user-agent").unwrap().starts_with(&format!("pi/{VERSION} ")));
    assert_eq!(requests[0].path(), "/api/models/providers/test-provider");
    assert_eq!(requests[0].query("types").as_deref(), Some("chat,image,classifier"));
    assert_eq!(requests[0].header("accept"), Some("application/json"));
}

#[tokio::test]
async fn overlays_image_and_classifier_models_and_drops_unknown_types() {
    let mut chat = model_json("test-provider", "chat");
    chat["type"] = json!("chat");
    let mut clip = model_json("test-provider", "clip");
    clip["type"] = json!("video");
    let body = json!({
        "chat": chat,
        "flux": {"type": "image", "id": "flux", "name": "FLUX", "api": "openrouter-images", "provider": "x",
                 "baseUrl": "https://example.test/v1", "input": ["text"], "output": ["image"],
                 "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}},
        "jev": {"type": "classifier", "id": "jev", "name": "Jev", "api": "typesafe-system-one",
                "provider": "test-provider", "baseUrl": "https://example.test/v1", "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 64000},
        "clip": clip,
    });
    let server = TestServer::start(move |_| TestReply::json(&body)).await;
    let overlay = overlay(&server, None);
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    assert_eq!(composed(&overlay), ["static", "chat", "flux", "jev"]);
    let all = overlay.compose(Vec::new());
    assert!(all.iter().all(|model| model.provider == "test-provider"));
    assert_eq!(all[1].model_type.as_deref(), Some("image"));
    assert_eq!(ids(&publisher.stored().unwrap().models), ["chat", "flux", "jev"]);
}

#[tokio::test]
async fn prefers_the_newer_of_the_bundled_and_remote_catalogs() {
    let local = parse_http_date("Thu, 23 Jul 2026 10:00:00 GMT").unwrap();
    let older = TestReply::json(&json!({"old": model_json("test-provider", "old")}))
        .header("last-modified", "Thu, 23 Jul 2026 09:59:00 GMT");
    let newer = TestReply::json(&json!({"newer": model_json("test-provider", "newer")}))
        .header("last-modified", "Thu, 23 Jul 2026 10:01:00 GMT");
    let server = scripted(vec![older, newer]).await;
    let overlay = overlay(&server, Some(local));
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    assert_eq!(composed(&overlay), ["static"]);
    refresh(&overlay, &publisher, true).await.unwrap();
    assert_eq!(composed(&overlay), ["static", "newer"]);
    assert_eq!(publisher.stored().unwrap().last_modified, Some(local + 60_000));
}

#[tokio::test]
async fn revalidates_with_the_etag_and_keeps_the_overlay_on_304() {
    let first = TestReply::json(&json!({"dynamic": model_json("test-provider", "dynamic")})).header("etag", "\"c-1\"");
    let server = scripted(vec![first, TestReply::status(304, "").header("etag", "\"c-1\"")]).await;
    let overlay = overlay(&server, None);
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    assert_eq!(server.requests()[0].header("if-none-match"), None);
    let checked_at = publisher.stored().unwrap().checked_at.unwrap();
    assert_eq!(publisher.stored().unwrap().etag.as_deref(), Some("\"c-1\""));

    let restarted = overlay_from(&server);
    refresh(&restarted, &publisher, true).await.unwrap();
    assert_eq!(server.requests()[1].header("if-none-match"), Some("\"c-1\""));
    assert_eq!(composed(&restarted), ["static", "dynamic"]);
    let stored = publisher.stored().unwrap();
    assert_eq!(ids(&stored.models), ["dynamic"]);
    assert_eq!(stored.etag.as_deref(), Some("\"c-1\""));
    assert!(stored.checked_at.unwrap() >= checked_at);
}

fn overlay_from(server: &TestServer) -> RemoteCatalogOverlay {
    overlay(server, None)
}

#[tokio::test]
async fn unavailable_overlays_drop_the_etag() {
    let first = TestReply::json(&json!({"dynamic": model_json("test-provider", "dynamic")})).header("etag", "\"c-1\"");
    let server = scripted(vec![first, TestReply::status(501, "not implemented")]).await;
    let overlay = overlay(&server, None);
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    refresh(&overlay, &publisher, true).await.unwrap();
    let stored = publisher.stored().unwrap();
    assert_eq!(stored.etag, None);
    assert_eq!(stored.last_modified, Some(0));
}

#[tokio::test]
async fn transient_failures_keep_the_etag_and_overlay() {
    let first = TestReply::json(&json!({"dynamic": model_json("test-provider", "dynamic")})).header("etag", "\"c-1\"");
    let limited = || TestReply::status(429, "rate limited");
    let not_modified = TestReply::status(304, "");
    let server = scripted(vec![first, limited(), limited(), limited(), not_modified]).await;
    let overlay = overlay(&server, None);
    let publisher = RecordingPublisher::new(None);
    refresh(&overlay, &publisher, false).await.unwrap();
    let error = refresh(&overlay, &publisher, true).await.unwrap_err();
    assert_eq!(error.to_string(), "Model catalog request failed for test-provider: 429");
    let stored = publisher.stored().unwrap();
    assert_eq!(stored.etag.as_deref(), Some("\"c-1\""));
    assert_eq!(ids(&stored.models), ["dynamic"]);

    refresh(&overlay, &publisher, true).await.unwrap();
    assert_eq!(server.requests()[4].header("if-none-match"), Some("\"c-1\""));
    assert_eq!(composed(&overlay), ["static", "dynamic"]);
}

#[tokio::test]
async fn offline_phases_only_restore_the_cache() {
    let server = scripted(Vec::new()).await;
    let overlay = overlay(&server, None);
    let mut entry = ModelsStoreEntry::with_models(vec![model("test-provider", "cached"), model("other", "x")]);
    entry.last_modified = Some(5);
    let publisher = RecordingPublisher::new(Some(entry));
    refresh_with(&overlay, &publisher, None, false, true).await.unwrap();
    assert_eq!(composed(&overlay), ["static", "cached"]);
    assert!(server.requests().is_empty());

    publisher.accept.store(false, std::sync::atomic::Ordering::SeqCst);
    let fresh = overlay_from(&server);
    refresh_with(&fresh, &publisher, None, false, false).await.unwrap();
    assert_eq!(composed(&fresh), ["static"], "a superseded refresh must not change the overlay");
}

#[test]
fn catalog_shapes_and_http_dates_parse() {
    let array = json!([model_json("p", "a"), {"name": "no id"}]);
    assert_eq!(ids(&parse_catalog_ok(&array)), ["a"]);
    let wrapped = json!({"models": [model_json("p", "b")]});
    assert_eq!(ids(&parse_catalog_ok(&wrapped)), ["b"]);
    assert!(crate::catalog::remote::parse_catalog("p", &json!("text")).is_err());
    assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:01 GMT"), Some(1000));
    assert_eq!(parse_http_date("Sat, 29 Feb 2020 12:30:45 GMT"), Some(1_582_979_445_000));
    assert_eq!(parse_http_date("yesterday"), None);
}

fn parse_catalog_ok(value: &serde_json::Value) -> Vec<pi_ai::Model> {
    let models = crate::catalog::remote::parse_catalog("q", value).unwrap();
    assert!(models.iter().all(|model| model.provider == "q"));
    models
}

fn gateway_model(id: &str) -> serde_json::Value {
    json!({"id": id, "name": id, "reasoning": false, "input": ["text"], "cost": {"input": 1, "output": 2},
        "contextWindow": 1000, "maxTokens": 100})
}

#[tokio::test]
async fn radius_gateway_catalog_replaces_the_baseline() {
    let server = TestServer::start(|request| match request.path() {
        "/v1/config" => TestReply::json(&json!({"baseUrl": "https://gw/v1", "models": [gateway_model("m1")]})),
        _ => TestReply::status(404, "{}"),
    })
    .await;
    let radius = Arc::new(RadiusCatalog::new("work", Some(&server.url)));
    let publisher = RecordingPublisher::new(None);
    let key = Credential::ApiKey(ApiKeyCredential { key: Some("tok".into()), env: None });
    refresh_with(radius.as_ref(), &publisher, Some(&key), true, false).await.unwrap();
    assert_eq!(server.requests()[0].header("authorization"), Some("Bearer tok"));
    assert_eq!(ids(&radius.models()), ["m1"]);
    assert_eq!(radius.models()[0].api, "pi-messages");
    assert_eq!(ids(&publisher.stored().unwrap().models), ["m1"]);

    let restored = RadiusCatalog::new("work", Some(&server.url));
    refresh_with(&restored, &publisher, None, false, false).await.unwrap();
    assert_eq!(ids(&restored.compose(Vec::new())), ["m1"]);
}

#[tokio::test]
async fn radius_imports_legacy_credential_catalogs() {
    let mut oauth = OAuthCredential { access: "token".into(), ..OAuthCredential::default() };
    oauth.extra.insert("gatewayConfig".into(), json!({"baseUrl": "https://gw/v1", "models": [gateway_model("old")]}));
    let radius = RadiusCatalog::new("radius", None);
    let publisher = RecordingPublisher::new(None);
    refresh_with(&radius, &publisher, Some(&Credential::OAuth(oauth)), false, false).await.unwrap();
    assert_eq!(ids(&radius.models()), ["old"]);
    assert!(publisher.stored().unwrap().checked_at.is_some());
}
