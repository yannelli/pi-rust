use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, expect_err, sequence};
use crate::auth::oauth::XaiOAuth;
use crate::auth::oauth::http::now_ms;
use crate::auth::{AuthError, AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

fn device_code(overrides: Value) -> Value {
    let mut body = json!({ "device_code": "device-code", "user_code": "ABCD-1234",
        "verification_uri": "https://accounts.x.ai/oauth2/device", "interval": 5, "expires_in": 900 });
    for (key, value) in overrides.as_object().unwrap() {
        body[key] = value.clone();
    }
    body
}

async fn mock(device: Value, polls: Vec<(u16, Value)>) -> MockServer {
    let poll = sequence(polls);
    MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/auth.x.ai/oauth2/device/code" => {
            assert_eq!(request.form("client_id").as_deref(), Some(CLIENT_ID));
            assert_eq!(
                request.form("scope").as_deref(),
                Some("openid profile email offline_access grok-cli:access api:access")
            );
            assert_eq!(request.form("referrer").as_deref(), Some("pi"));
            Reply::Json(200, device.clone())
        }
        "/auth.x.ai/oauth2/token" => poll(),
        other => panic!("unexpected request {other}"),
    })
    .await
}

fn flow(server: &MockServer) -> XaiOAuth {
    XaiOAuth { hooks: server.hooks() }
}

fn no_prompts() -> TestInteraction {
    TestInteraction::new(|prompt, _, _| panic!("unexpected prompt {prompt:?}"))
}

fn device_event(interaction: &TestInteraction) -> AuthEvent {
    interaction.events().into_iter().find(|event| matches!(event, AuthEvent::DeviceCode { .. })).expect("device code")
}

#[tokio::test(start_paused = true)]
async fn device_login_delays_polling_and_handles_pending_and_slow_down() {
    let token = json!({ "access_token": "access-token", "refresh_token": "refresh-token", "expires_in": 21600 });
    let polls = vec![
        (400, json!({ "error": "authorization_pending" })),
        (400, json!({ "error": "slow_down", "interval": 10 })),
        (200, token),
    ];
    let server = mock(device_code(json!({})), polls).await;
    let interaction = no_prompts();
    let start = Instant::now();
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    let times: Vec<u128> = server.requests_to("/oauth2/token").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [5000, 10000, 20000]);
    let poll = &server.requests_to("/oauth2/token")[0];
    assert_eq!(poll.form("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
    assert_eq!(poll.form("device_code").as_deref(), Some("device-code"));
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), ("access-token", "refresh-token"));
    assert!((credential.expires - (now_ms() + 21_600_000 - 300_000)).abs() < 5_000);
    assert!(credential.extra.is_empty());
    let expected = AuthEvent::DeviceCode {
        user_code: "ABCD-1234".into(),
        verification_uri: "https://accounts.x.ai/oauth2/device".into(),
        interval_seconds: Some(5),
        expires_in_seconds: Some(900),
    };
    assert_eq!(device_event(&interaction), expected);
}

#[tokio::test(start_paused = true)]
async fn zero_interval_uses_the_default_and_the_complete_uri_is_preferred() {
    let device =
        device_code(json!({ "interval": 0, "verification_uri_complete": "https://accounts.x.ai/device?code=ABCD" }));
    let server = mock(device, vec![(200, json!({ "access_token": "a", "refresh_token": "r" }))]).await;
    let interaction = no_prompts();
    let start = Instant::now();
    flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!((server.requests_to("/oauth2/token")[0].at - start).as_millis(), 5000);
    let AuthEvent::DeviceCode { verification_uri, interval_seconds, .. } = device_event(&interaction) else {
        unreachable!()
    };
    assert_eq!((verification_uri.as_str(), interval_seconds), ("https://accounts.x.ai/device?code=ABCD", None));
}

#[tokio::test(start_paused = true)]
async fn untrusted_uris_denials_and_cancellation_fail_login() {
    for uri in ["http://accounts.x.ai/oauth2/device", "file:///etc/passwd", "not a url"] {
        let server = mock(device_code(json!({ "verification_uri": uri })), vec![]).await;
        let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
        assert_eq!(expect_err(result), "Untrusted verification URI in xAI OAuth response");
    }
    let server = mock(device_code(json!({ "verification_uri_complete": "http://x" })), vec![]).await;
    let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Untrusted verification URI in xAI OAuth response");

    for error in ["access_denied", "authorization_denied"] {
        let server = mock(device_code(json!({ "interval": 1 })), vec![(400, json!({ "error": error }))]).await;
        let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
        assert_eq!(expect_err(result), "xAI device authorization was denied");
    }
    let server = mock(device_code(json!({})), vec![(400, json!({ "error": "expired_token" }))]).await;
    let result = flow(&server).login(&no_prompts(), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "xAI device code expired");

    let server = mock(device_code(json!({})), vec![]).await;
    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let interaction = no_prompts().on_notify(move |_| stop.cancel());
    let result = flow(&server).login(&interaction, &LoginOptions::default(), cancel).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
    assert_eq!(server.requests().len(), 1);
}

async fn refresh(server: &MockServer, token: &str) -> crate::auth::AuthResult<OAuthCredential> {
    let credential = OAuthCredential { refresh: token.into(), ..OAuthCredential::default() };
    flow(server).refresh(&credential, CancellationToken::new()).await
}

#[tokio::test]
async fn refresh_preserves_an_unrotated_token_and_reports_failures() {
    let replies = vec![
        (200, json!({ "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 60 })),
        (200, json!({ "access_token": "newer-access" })),
        (200, json!({ "refresh_token": "r" })),
        (400, json!({ "error": "invalid_grant", "error_description": "refresh token revoked" })),
    ];
    let reply = sequence(replies);
    let server = MockServer::start(move |request: &Recorded| {
        assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(request.form("client_id").as_deref(), Some(CLIENT_ID));
        reply()
    })
    .await;
    let rotated = refresh(&server, "old-refresh").await.unwrap();
    assert_eq!((rotated.access.as_str(), rotated.refresh.as_str()), ("new-access", "new-refresh"));
    let preserved = refresh(&server, "keep-refresh").await.unwrap();
    assert_eq!((preserved.access.as_str(), preserved.refresh.as_str()), ("newer-access", "keep-refresh"));
    // A missing expires_in means a one-hour lifetime.
    assert!((preserved.expires - (now_ms() + 3_600_000 - 300_000)).abs() < 5_000);
    assert_eq!(flow(&server).to_auth(&preserved).api_key.as_deref(), Some("newer-access"));
    assert_eq!(expect_err(refresh(&server, "x").await), "Invalid xAI OAuth response field: access_token");
    let error = expect_err(refresh(&server, "x").await);
    assert_eq!(error, "xAI OAuth token refresh failed (HTTP 400): invalid_grant: refresh token revoked");
    let requests = server.requests();
    assert_eq!(requests[0].form("refresh_token").as_deref(), Some("old-refresh"));

    let html = MockServer::start(|_: &Recorded| Reply::Text(502, "<html>".into())).await;
    assert_eq!(expect_err(refresh(&html, "x").await), "xAI OAuth returned invalid JSON (HTTP 502)");
    let oauth = XaiOAuth::default();
    assert_eq!(
        (oauth.name(), oauth.login_label()),
        ("xAI (Grok/X subscription)", Some("Sign in with SuperGrok or X Premium"))
    );
}
