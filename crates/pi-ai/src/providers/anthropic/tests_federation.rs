//! Workload identity federation, ported from upstream `anthropic-federation.test.ts` against a
//! local token endpoint instead of a mocked SDK.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::AnthropicMessages;
use super::tests::{API, context, message_end, message_start};
use crate::providers::mock_server::{MockResponse, MockServer, collect, test_model};
use crate::registry::{ApiProvider, StreamOptions};
use crate::types::StopReason;

struct Federation {
    server: MockServer,
    env: BTreeMap<String, String>,
    _token_file: tempfile::NamedTempFile,
}

fn sse() -> MockResponse {
    let mut frames = vec![message_start(json!({"input_tokens": 1}))];
    frames.extend(message_end("end_turn", json!({"output_tokens": 1})));
    MockResponse::Sse(frames)
}

/// A token endpoint and messages endpoint; `messages` picks the reply for the n-th messages request.
async fn federation(messages: impl Fn(usize) -> MockResponse + Send + Sync + 'static) -> Federation {
    let tokens = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let server = MockServer::routed(move |request| {
        if request.path == "/v1/oauth/token" {
            let count = tokens.fetch_add(1, Ordering::SeqCst) + 1;
            return MockResponse::json(
                &json!({"access_token": format!("tok-{count}"), "token_type": "Bearer", "expires_in": 3600}),
            );
        }
        messages(calls.fetch_add(1, Ordering::SeqCst))
    })
    .await;
    let token_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token_file.path(), "jwt-token\n").unwrap();
    let env = [
        ("ANTHROPIC_FEDERATION_RULE_ID", "fdrl_test"),
        ("ANTHROPIC_ORGANIZATION_ID", "org-test"),
        ("ANTHROPIC_SERVICE_ACCOUNT_ID", "svac_test"),
        ("ANTHROPIC_IDENTITY_TOKEN_FILE", token_file.path().to_str().unwrap()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    Federation { server, env, _token_file: token_file }
}

#[tokio::test]
async fn exchanges_the_identity_token_and_caches_the_access_token() {
    let mut federation = federation(|_| sse()).await;
    let model = test_model(API, "anthropic", &federation.server.base_url);
    let options = StreamOptions { env: federation.env.clone(), ..StreamOptions::default() };
    for _ in 0..2 {
        let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options.clone())).await;
        assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    }
    let exchange = federation.server.request().await;
    assert_eq!(exchange.path, "/v1/oauth/token");
    assert_eq!(exchange.headers["anthropic-beta"], "oauth-2025-04-20,oidc-federation-2026-04-01");
    assert_eq!(
        exchange.body,
        json!({"grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion": "jwt-token",
            "federation_rule_id": "fdrl_test", "organization_id": "org-test", "service_account_id": "svac_test"})
    );
    for _ in 0..2 {
        let request = federation.server.request().await;
        assert_eq!(request.path, "/v1/messages");
        assert_eq!(request.headers["authorization"], "Bearer tok-1");
        assert!(request.headers["anthropic-beta"].split(',').any(|feature| feature == "oauth-2025-04-20"));
        assert!(!request.headers.contains_key("x-api-key"));
    }
}

#[tokio::test]
async fn a_rejected_token_is_exchanged_again_once() {
    let mut federation = federation(|call| match call {
        0 => MockResponse::Status(401, json!({"error": {"message": "expired"}}).to_string()),
        _ => sse(),
    })
    .await;
    let model = test_model(API, "anthropic", &federation.server.base_url);
    let options = StreamOptions { env: federation.env.clone(), ..StreamOptions::default() };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let mut seen = Vec::new();
    for _ in 0..4 {
        let request = federation.server.request().await;
        seen.push((request.path.clone(), request.headers.get("authorization").cloned()));
    }
    assert_eq!(seen[1], ("/v1/messages".to_string(), Some("Bearer tok-1".to_string())));
    assert_eq!(seen[3], ("/v1/messages".to_string(), Some("Bearer tok-2".to_string())));
}

#[tokio::test]
async fn api_keys_win_and_other_providers_are_not_federated() {
    let mut federation = federation(|_| sse()).await;
    let model = test_model(API, "anthropic", &federation.server.base_url);
    let options =
        StreamOptions { api_key: Some("explicit-key".into()), env: federation.env.clone(), ..Default::default() };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop);
    let request = federation.server.request().await;
    assert_eq!((request.path.as_str(), request.headers["x-api-key"].as_str()), ("/v1/messages", "explicit-key"));
    let kimi = test_model(API, "kimi-coding", &federation.server.base_url);
    let options = StreamOptions { env: federation.env.clone(), ..Default::default() };
    let (_, message) = collect(AnthropicMessages.stream(&kimi, &context(), options)).await;
    assert_eq!(message.error_message.as_deref(), Some("No API key for provider: kimi-coding"));
}

#[tokio::test]
async fn token_endpoint_failures_explain_the_federation_rule() {
    let server = MockServer::routed(|_| {
        let body = json!({"error": "invalid_grant", "assertion": "secret"}).to_string();
        MockResponse::with_headers(401, &[("request-id", "req_9")], body)
    })
    .await;
    let token_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(token_file.path(), "jwt").unwrap();
    let env = BTreeMap::from([
        ("ANTHROPIC_FEDERATION_RULE_ID".to_string(), "fdrl".to_string()),
        ("ANTHROPIC_ORGANIZATION_ID".to_string(), "org".to_string()),
        ("ANTHROPIC_IDENTITY_TOKEN_FILE".to_string(), token_file.path().to_str().unwrap().to_string()),
    ]);
    let model = test_model(API, "anthropic", &server.base_url);
    let (_, message) =
        collect(AnthropicMessages.stream(&model, &context(), StreamOptions { env, ..Default::default() })).await;
    let error = message.error_message.unwrap();
    assert!(
        error.starts_with("Token exchange failed with status 401 (request-id req_9): {\"error\":\"invalid_grant\"}")
    );
    assert!(error.contains("set the ANTHROPIC_WORKSPACE_ID environment variable"));
    assert!(!error.contains("secret"));
}

/// Upstream's `PiAnthropic` turns off the SDK default credential chain (`ANTHROPIC_PROFILE`,
/// `ANTHROPIC_CONFIG_DIR` profiles), so profile files never supply auth.
#[tokio::test]
async fn does_not_run_the_sdk_profile_credential_chain() {
    let mut federation = federation(|_| sse()).await;
    let config_dir = tempfile::tempdir().unwrap();
    for dir in ["configs", "credentials"] {
        std::fs::create_dir_all(config_dir.path().join(dir)).unwrap();
    }
    let token_file = config_dir.path().join("credentials/work.json");
    std::fs::write(&token_file, json!({"type": "oauth_token", "access_token": "profile-token"}).to_string()).unwrap();
    let config = json!({"authentication": {"type": "user_oauth", "credentials_path": token_file}});
    std::fs::write(config_dir.path().join("configs/work.json"), config.to_string()).unwrap();
    let profile_env = BTreeMap::from([
        ("ANTHROPIC_PROFILE".to_string(), "work".to_string()),
        ("ANTHROPIC_CONFIG_DIR".to_string(), config_dir.path().to_str().unwrap().to_string()),
    ]);
    let model = test_model(API, "anthropic", &federation.server.base_url);
    let mut env = federation.env.clone();
    env.extend(profile_env.clone());
    let headers = BTreeMap::from([("Authorization".to_string(), "Bearer auth-token".to_string())]);
    let options = StreamOptions { env, headers, ..Default::default() };
    let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options)).await;
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let request = federation.server.request().await;
    assert_eq!(
        (request.path.as_str(), request.headers["authorization"].as_str()),
        ("/v1/messages", "Bearer auth-token")
    );
    let options = StreamOptions { env: profile_env, ..Default::default() };
    if std::env::var("ANTHROPIC_FEDERATION_RULE_ID").is_err() {
        let (_, message) = collect(AnthropicMessages.stream(&model, &context(), options)).await;
        assert_eq!(message.error_message.as_deref(), Some("No API key for provider: anthropic"));
    }
}
