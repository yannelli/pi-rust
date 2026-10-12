use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, answer, expect_err, sequence};
use crate::auth::oauth::GitHubCopilotOAuth;
use crate::auth::{AuthEvent, AuthPrompt, LoginOptions, OAuthAuth, OAuthCredential};

const TOKEN: &str = "tid=test;exp=9999999999;proxy-ep=proxy.individual.githubcopilot.com;";
const MODELS: &str = "/api.individual.githubcopilot.com/models";
const KNOWN: [&str; 4] = ["known-a", "known-b", "known-c", "known-d"];

type Route = Box<dyn Fn(&Recorded) -> Reply + Send + Sync>;

struct Routes {
    device: Value,
    polls: Route,
    models: Route,
    policy: Route,
}

impl Default for Routes {
    fn default() -> Self {
        Self {
            device: json!({ "device_code": "device-code", "user_code": "ABCD-EFGH",
                "verification_uri": "https://github.com/login/device", "interval": 1, "expires_in": 900 }),
            polls: Box::new(|_| Reply::Json(200, json!({ "access_token": "ghu_refresh_token" }))),
            models: Box::new(|_| Reply::Json(200, json!({ "data": [] }))),
            policy: Box::new(|request| panic!("unexpected policy request {}", request.path)),
        }
    }
}

async fn mock(routes: Routes) -> MockServer {
    MockServer::start(move |request: &Recorded| {
        let path = request.path.as_str();
        if path.ends_with("/login/device/code") {
            Reply::Json(200, routes.device.clone())
        } else if path.ends_with("/login/oauth/access_token") {
            (routes.polls)(request)
        } else if path.ends_with("/copilot_internal/v2/token") {
            Reply::Json(200, json!({ "token": TOKEN, "expires_at": 9999999999u64 }))
        } else if path.ends_with("/models") {
            (routes.models)(request)
        } else if path.ends_with("/policy") {
            (routes.policy)(request)
        } else {
            panic!("unexpected request {path}")
        }
    })
    .await
}

fn flow(server: &MockServer) -> GitHubCopilotOAuth {
    GitHubCopilotOAuth { hooks: server.hooks(), known_model: Arc::new(|id| KNOWN.contains(&id)) }
}

fn interaction(domain: &'static str) -> TestInteraction {
    TestInteraction::new(move |prompt, _, _| match prompt {
        AuthPrompt::Text { message, .. } if message == "GitHub Enterprise URL/domain (blank for github.com)" => {
            answer(domain)
        }
        other => panic!("unexpected prompt {other:?}"),
    })
}

fn catalog(models: Value) -> Route {
    Box::new(move |request| {
        assert_eq!(request.header("authorization"), Some(format!("Bearer {TOKEN}").as_str()));
        assert_eq!(request.header("x-github-api-version"), Some("2026-06-01"));
        Reply::Json(200, json!({ "data": models }))
    })
}

fn model(id: &str, picker: bool, state: &str) -> Value {
    json!({ "id": id, "model_picker_enabled": picker, "policy": { "state": state } })
}

fn policy_ids(server: &MockServer) -> Vec<String> {
    let requests = server.requests_to("/policy");
    let id = |r: &Recorded| r.path.trim_end_matches("/policy").rsplit('/').next().unwrap().to_string();
    requests.iter().map(id).collect()
}

async fn login(server: &MockServer, domain: &'static str) -> crate::auth::AuthResult<OAuthCredential> {
    flow(server).login(&interaction(domain), &LoginOptions::default(), CancellationToken::new()).await
}

#[tokio::test(start_paused = true)]
async fn login_keeps_picker_models_and_enables_known_unconfigured_ones() {
    let mut no_tools = model("known-c", true, "unconfigured");
    no_tools["capabilities"] = json!({ "supports": { "tool_calls": false } });
    let models = json!([
        model("known-a", true, "enabled"),
        model("known-b", true, "unconfigured"),
        model("remote-only", true, "unconfigured"),
        no_tools,
        model("off", true, "disabled")
    ]);
    let server = mock(Routes {
        models: catalog(models),
        policy: Box::new(|request| {
            assert_eq!(request.json(), json!({ "state": "enabled" }));
            assert_eq!(request.header("openai-intent"), Some("chat-policy"));
            Reply::Text(200, String::new())
        }),
        ..Routes::default()
    })
    .await;
    let credential = login(&server, "").await.unwrap();
    assert_eq!(policy_ids(&server), ["known-b"]);
    assert_eq!(credential.extra["availableModelIds"], json!(["known-a", "known-b", "remote-only"]));
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), (TOKEN, "ghu_refresh_token"));
    assert_eq!(credential.expires, 9999999999 * 1000 - 5 * 60 * 1000);
    assert!(credential.extra.get("enterpriseUrl").is_none());
    let device_code = &server.requests_to("/login/device/code")[0];
    assert_eq!(device_code.path, "/github.com/login/device/code");
    assert_eq!(device_code.form("scope").as_deref(), Some("read:user"));
    assert_eq!(device_code.form("client_id").as_deref(), Some("Iv1.b507a08c87ecfe98"));
    assert_eq!(device_code.header("user-agent"), Some("GitHubCopilotChat/0.35.0"));
}

#[tokio::test(start_paused = true)]
async fn individual_accounts_fall_back_to_enabled_policies_when_the_picker_is_empty() {
    let models = json!([model("known-a", false, "enabled"), model("known-b", false, "unconfigured")]);
    let server = mock(Routes {
        models: catalog(models),
        policy: Box::new(|_| Reply::Text(200, String::new())),
        ..Routes::default()
    })
    .await;
    let credential = login(&server, "").await.unwrap();
    assert_eq!(credential.extra["availableModelIds"], json!(["known-a", "known-b"]));
}

#[tokio::test(start_paused = true)]
async fn policy_updates_retry_after_429_continue_after_transport_errors_and_stop_past_the_budget() {
    let replies = Mutex::new(vec![
        Reply::JsonHeaders(429, json!({}), vec![("retry-after", "1".into())]),
        Reply::Text(200, String::new()),
        Reply::Close,
        Reply::JsonHeaders(429, json!({}), vec![("retry-after", "5".into())]),
    ]);
    let ids = ["known-a", "known-b", "known-c", "known-d"];
    let models = Value::Array(ids.iter().map(|id| model(id, false, "unconfigured")).collect());
    let server = mock(Routes {
        models: catalog(models),
        policy: Box::new(move |_| replies.lock().unwrap().remove(0)),
        ..Routes::default()
    })
    .await;
    let credential = login(&server, "").await.unwrap();
    let policies = server.requests_to("/policy");
    // known-b fails in transport and is skipped; known-c's Retry-After exceeds the 5 second
    // budget, which stops the batch before known-d. Login still succeeds.
    assert_eq!(policy_ids(&server), ["known-a", "known-a", "known-b", "known-c"]);
    assert_eq!((policies[1].at - policies[0].at).as_millis(), 1000);
    assert_eq!(credential.extra["availableModelIds"], json!(["known-a"]));
}

#[tokio::test(start_paused = true)]
async fn device_polling_waits_first_and_honors_slow_down_intervals() {
    let polls = sequence(vec![
        (200, json!({ "error": "authorization_pending" })),
        (200, json!({ "error": "slow_down", "interval": 7 })),
        (200, json!({ "access_token": "ghu_refresh_token" })),
    ]);
    let mut routes = Routes { polls: Box::new(move |_| polls()), ..Routes::default() };
    routes.device["interval"] = json!(5);
    let server = mock(routes).await;
    let start = Instant::now();
    login(&server, "").await.unwrap();
    let times: Vec<u128> = server.requests_to("/access_token").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [5000, 10000, 17000]);
    let poll = &server.requests_to("/access_token")[0];
    assert_eq!(poll.form("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
    assert_eq!(poll.form("device_code").as_deref(), Some("device-code"));
}

#[tokio::test(start_paused = true)]
async fn device_polling_times_out_after_slow_down_and_reports_failures() {
    let polls = sequence(vec![(200, json!({ "error": "slow_down" })), (200, json!({ "error": "slow_down" }))]);
    let mut routes = Routes { polls: Box::new(move |_| polls()), ..Routes::default() };
    routes.device["interval"] = json!(5);
    routes.device["expires_in"] = json!(25);
    let server = mock(routes).await;
    let start = Instant::now();
    let error = expect_err(login(&server, "").await);
    assert!(error.starts_with("Device flow timed out after one or more slow_down responses"), "{error}");
    let times: Vec<u128> = server.requests_to("/access_token").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [5000, 15000]);

    let denied =
        Box::new(|_: &Recorded| Reply::Json(200, json!({ "error": "access_denied", "error_description": "no" })));
    let server = mock(Routes { polls: denied, ..Routes::default() }).await;
    assert_eq!(expect_err(login(&server, "").await), "Device flow failed: access_denied: no");
}

#[tokio::test(start_paused = true)]
async fn verification_uris_must_be_http_and_are_normalized() {
    let mut routes = Routes::default();
    routes.device["verification_uri"] = json!("file:///bin/sh");
    let server = mock(routes).await;
    assert_eq!(expect_err(login(&server, "").await), "Untrusted verification_uri in device code response");

    let mut routes = Routes::default();
    routes.device["verification_uri"] = json!("HTTPS://GitHub.com/login/device");
    let server = mock(routes).await;
    let interaction = interaction("");
    flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    let event = interaction.events().into_iter().find(|event| matches!(event, AuthEvent::DeviceCode { .. })).unwrap();
    let AuthEvent::DeviceCode { verification_uri, user_code, interval_seconds, expires_in_seconds } = event else {
        unreachable!()
    };
    assert_eq!(verification_uri, "https://github.com/login/device");
    assert_eq!((user_code.as_str(), interval_seconds, expires_in_seconds), ("ABCD-EFGH", Some(1), Some(900)));
}

#[tokio::test(start_paused = true)]
async fn enterprise_logins_use_the_enterprise_hosts() {
    let server = mock(Routes::default()).await;
    let credential = login(&server, " https://company.ghe.com/path ").await.unwrap();
    assert_eq!(credential.extra_str("enterpriseUrl"), Some("company.ghe.com"));
    assert_eq!(server.requests_to("/login/device/code")[0].path, "/company.ghe.com/login/device/code");
    assert_eq!(server.requests_to("/v2/token")[0].path, "/api.company.ghe.com/copilot_internal/v2/token");
    assert_eq!(expect_err(login(&server, "https://").await), "Invalid GitHub Enterprise URL/domain");
}

#[tokio::test(start_paused = true)]
async fn refresh_keeps_the_enterprise_domain_and_does_not_retry_catalog_throttling() {
    let server =
        mock(Routes { models: catalog(json!([model("known-a", true, "enabled")])), ..Routes::default() }).await;
    let mut old = OAuthCredential { access: "old".into(), refresh: "gh-token".into(), ..Default::default() };
    old.extra.insert("enterpriseUrl".into(), json!("company.ghe.com"));
    let refreshed = flow(&server).refresh(&old, CancellationToken::new()).await.unwrap();
    assert_eq!((refreshed.access.as_str(), refreshed.refresh.as_str()), (TOKEN, "gh-token"));
    assert_eq!(refreshed.extra_str("enterpriseUrl"), Some("company.ghe.com"));
    assert_eq!(refreshed.extra["availableModelIds"], json!(["known-a"]));
    let token_request = &server.requests_to("/v2/token")[0];
    assert_eq!(token_request.path, "/api.company.ghe.com/copilot_internal/v2/token");
    assert_eq!(token_request.header("authorization"), Some("Bearer gh-token"));
    assert_eq!(token_request.header("editor-version"), Some("vscode/1.107.0"));

    let throttled = Box::new(|_: &Recorded| Reply::JsonHeaders(429, json!({}), vec![("retry-after", "0".into())]));
    let server = mock(Routes { models: throttled, ..Routes::default() }).await;
    let error = expect_err(flow(&server).refresh(&old, CancellationToken::new()).await);
    assert!(error.starts_with("429"), "{error}");
    assert_eq!(server.requests_to(MODELS).len(), 1);
}

#[test]
fn to_auth_sets_the_per_credential_base_url() {
    let oauth = GitHubCopilotOAuth::default();
    let mut credential =
        OAuthCredential { access: "tid=a;proxy-ep=proxy.business.githubcopilot.com;x".into(), ..Default::default() };
    let auth = oauth.to_auth(&credential);
    assert_eq!(auth.api_key.as_deref(), Some(credential.access.as_str()));
    assert_eq!(auth.base_url.as_deref(), Some("https://api.business.githubcopilot.com"));
    credential.access = "no-proxy-ep".into();
    credential.extra.insert("enterpriseUrl".into(), json!("https://company.ghe.com"));
    assert_eq!(oauth.to_auth(&credential).base_url.as_deref(), Some("https://copilot-api.company.ghe.com"));
    credential.extra.clear();
    assert_eq!(oauth.to_auth(&credential).base_url.as_deref(), Some("https://api.individual.githubcopilot.com"));
}
