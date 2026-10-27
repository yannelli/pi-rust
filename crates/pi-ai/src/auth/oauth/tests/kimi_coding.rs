use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, expect_err, sequence};
use crate::auth::oauth::KimiCodingOAuth;
use crate::auth::{AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

const CLIENT_ID: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";

fn device_authorization(overrides: Value) -> Value {
    let mut body = json!({ "user_code": "ABCD-1234", "device_code": "device-code-123",
        "verification_uri": "https://www.kimi.com/code",
        "verification_uri_complete": "https://www.kimi.com/code?user_code=ABCD-1234", "interval": 5, "expires_in": 600 });
    for (key, value) in overrides.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

async fn mock(device: Value, polls: Vec<(u16, Value)>) -> MockServer {
    let poll = sequence(polls);
    MockServer::start(move |request: &Recorded| {
        assert_eq!(request.form("client_id").as_deref(), Some(CLIENT_ID));
        if request.path.ends_with("/api/oauth/device_authorization") {
            Reply::Json(200, device.clone())
        } else {
            assert!(request.path.ends_with("/api/oauth/token"), "{}", request.path);
            poll()
        }
    })
    .await
}

fn flow(server: &MockServer) -> KimiCodingOAuth {
    KimiCodingOAuth { hooks: server.hooks() }
}

fn no_prompts() -> TestInteraction {
    TestInteraction::new(|_, _, _| panic!("Kimi Code login should not prompt"))
}

#[tokio::test(start_paused = true)]
async fn device_login_polls_until_tokens_arrive() {
    let token = json!({ "access_token": "access", "refresh_token": "refresh", "expires_in": 3600 });
    let polls =
        vec![(400, json!({ "error": "authorization_pending" })), (400, json!({ "error": "slow_down" })), (200, token)];
    let server = mock(device_authorization(json!({})), polls).await;
    let interaction = no_prompts();
    let start = Instant::now();
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), ("access", "refresh"));
    let polls = server.requests_to("/api/oauth/token");
    let times: Vec<u128> = polls.iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [5000, 10000, 20000]);
    assert_eq!(polls[0].path, "/auth.kimi.com/api/oauth/token");
    assert_eq!(polls[0].form("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
    assert_eq!(polls[0].form("device_code").as_deref(), Some("device-code-123"));
    let expected = AuthEvent::DeviceCode {
        user_code: "ABCD-1234".into(),
        verification_uri: "https://www.kimi.com/code?user_code=ABCD-1234".into(),
        interval_seconds: Some(5),
        expires_in_seconds: Some(600),
        open_browser_on_confirm: None,
        copy_code_on_confirm: None,
    };
    assert_eq!(interaction.events(), [expected]);
}

#[tokio::test(start_paused = true)]
async fn device_login_fails_on_expiry_denial_and_bad_responses() {
    let cases = [
        (json!({ "error": "expired_token" }), "Kimi Code device authorization expired. Please restart login."),
        (json!({ "error": "access_denied" }), "Kimi Code login was denied."),
        (
            json!({ "error": "bad", "error_description": "why" }),
            "Kimi Code device token request failed (status 400): bad: why",
        ),
    ];
    for (reply, message) in cases {
        let server = mock(device_authorization(json!({ "interval": 1 })), vec![(400, reply)]).await;
        let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
        assert_eq!(expect_err(result), message);
    }
    let server = mock(device_authorization(json!({})), vec![(503, json!("down"))]).await;
    let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), r#"Kimi Code device token request failed with status 503: "down""#);
    let server = mock(device_authorization(json!({ "verification_uri": "file:///x" })), vec![]).await;
    let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
    assert!(expect_err(result).starts_with("Invalid Kimi Code device authorization response: {"));
}

#[tokio::test(start_paused = true)]
async fn kimi_code_oauth_host_overrides_the_default() {
    let token = json!({ "access_token": "a", "refresh_token": "r", "expires_in": 60 });
    let server = mock(device_authorization(json!({ "interval": 1 })), vec![(200, token)]).await;
    let mut hooks = server.hooks();
    hooks.env = Some(Arc::new(|name| (name == "KIMI_CODE_OAUTH_HOST").then(|| "https://auth.example.com/".into())));
    let flow = KimiCodingOAuth { hooks };
    let credential = flow.login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(credential.access, "a");
    let paths: Vec<String> = server.requests().into_iter().map(|request| request.path).collect();
    assert_eq!(paths, ["/auth.example.com/api/oauth/device_authorization", "/auth.example.com/api/oauth/token"]);
}

fn stored(refresh: &str) -> OAuthCredential {
    OAuthCredential { access: "old-access".into(), refresh: refresh.into(), expires: 0, ..Default::default() }
}

#[tokio::test(start_paused = true)]
async fn refresh_returns_a_bearer_header_and_retries_throttling() {
    let replies = vec![
        (200, json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600 })),
        (429, json!({ "error": "temporarily_unavailable" })),
        (500, json!({})),
        (200, json!({ "access_token": "a", "refresh_token": "r", "expires_in": 60 })),
        (400, json!({ "error": "invalid_grant", "error_description": "revoked" })),
        (429, json!({})),
        (429, json!({})),
        (429, json!({})),
        (429, json!({ "error": "busy" })),
    ];
    let reply = sequence(replies);
    let server = MockServer::start(move |request: &Recorded| {
        assert_eq!(request.path, "/auth.kimi.com/api/oauth/token");
        assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
        reply()
    })
    .await;
    let before = crate::auth::oauth::http::now_ms();
    let credential = flow(&server).refresh(&stored("old-refresh"), CancellationToken::new()).await.unwrap();
    assert!(credential.expires >= before + 3_600_000 && credential.extra.is_empty());
    assert_eq!(server.requests()[0].form("refresh_token").as_deref(), Some("old-refresh"));
    let headers = BTreeMap::from([("Authorization".to_string(), "Bearer new-access".to_string())]);
    assert_eq!(flow(&server).to_auth(&credential).headers, headers);
    assert_eq!(flow(&server).to_auth(&credential).api_key, None);

    let start = Instant::now();
    let retried = flow(&server).refresh(&stored("old"), CancellationToken::new()).await.unwrap();
    assert_eq!(retried.access, "a");
    let times: Vec<u128> = server.requests()[1..4].iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [0, 1000, 3000]);

    let error = expect_err(flow(&server).refresh(&stored("old"), CancellationToken::new()).await);
    assert_eq!(error, "Kimi Code token refresh unauthorized (status 400): revoked");
    let error = expect_err(flow(&server).refresh(&stored("old"), CancellationToken::new()).await);
    assert_eq!(error, r#"Kimi Code token refresh failed with status 429: {"error":"busy"}"#);
}
