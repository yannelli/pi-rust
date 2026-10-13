use serde_json::json;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, expect_err, sequence};
use crate::auth::oauth::MetaOAuth;
use crate::auth::oauth::http::now_ms;
use crate::auth::{AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

const CLIENT_ID: &str = "1031625952748946";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn flow(server: &MockServer) -> MetaOAuth {
    MetaOAuth { hooks: server.hooks() }
}

fn identity() -> OAuthCredential {
    OAuthCredential { refresh: "identity-token".into(), access: "LLM|old-key".into(), expires: 1, ..Default::default() }
}

#[tokio::test(start_paused = true)]
async fn login_polls_for_an_identity_token_and_mints_a_key() {
    let poll = sequence(vec![
        (400, json!({ "error": "authorization_pending" })),
        (200, json!({ "access_token": "identity-token", "token_type": "Bearer" })),
    ]);
    let server = MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/auth.meta.com/oidc/device/authorization/" => {
            assert_eq!(request.form("client_id").as_deref(), Some(CLIENT_ID));
            Reply::Json(
                200,
                json!({ "device_code": "device-code-123", "user_code": "ABCD-1234",
                "verification_uri": "https://auth.meta.com/oauth/device/",
                "verification_uri_complete": "https://auth.meta.com/oauth/device/?code=ABCD-1234",
                "interval": 5, "expires_in": 600 }),
            )
        }
        "/auth.meta.com/oidc/device/token/" => {
            assert_eq!(request.form("grant_type").as_deref(), Some("urn:ietf:params:oauth:grant-type:device_code"));
            assert_eq!(request.form("device_code").as_deref(), Some("device-code-123"));
            assert_eq!(request.form("client_id").as_deref(), Some(CLIENT_ID));
            poll()
        }
        "/api.meta.ai/muse-code/key" => {
            assert_eq!(request.header("authorization"), Some("Bearer identity-token"));
            assert_eq!((request.header("x-api-version"), request.body.as_str()), (Some("1.0.0"), "{}"));
            Reply::Json(200, json!({ "api_key": "LLM|minted-key" }))
        }
        other => panic!("unexpected request {other}"),
    })
    .await;
    let interaction = TestInteraction::new(|_, _, _| panic!("Meta login should not prompt"));
    let start = Instant::now();
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!((credential.refresh.as_str(), credential.access.as_str()), ("identity-token", "LLM|minted-key"));
    assert!((credential.expires - (now_ms() + DAY_MS)).abs() < 5_000);
    let times: Vec<u128> = server.requests_to("/device/token/").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(times, [5000, 10000]);
    let events = interaction.events();
    assert_eq!(
        events[0],
        AuthEvent::DeviceCode {
            user_code: "ABCD-1234".into(),
            verification_uri: "https://auth.meta.com/oauth/device/?code=ABCD-1234".into(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(600),
        }
    );
    assert_eq!(events[1], AuthEvent::Progress { message: "Enabling Meta Model API access...".into() });
}

#[tokio::test]
async fn refresh_re_mints_and_reports_setup_and_expired_sessions() {
    let reply = sequence(vec![
        (200, json!({ "api_key": "LLM|fresh-key" })),
        (200, json!({ "require_payment": true, "action_url": "https://dev.meta.ai/billing" })),
        (401, json!({ "detail": " Session invalid " })),
        (500, json!({ "message": "oops" })),
    ]);
    let server = MockServer::start(move |request: &Recorded| {
        assert_eq!(request.path, "/api.meta.ai/muse-code/key");
        assert_eq!(request.header("authorization"), Some("Bearer identity-token"));
        reply()
    })
    .await;
    let refreshed = flow(&server).refresh(&identity(), CancellationToken::new()).await.unwrap();
    assert_eq!((refreshed.refresh.as_str(), refreshed.access.as_str()), ("identity-token", "LLM|fresh-key"));
    assert!((refreshed.expires - (now_ms() + DAY_MS)).abs() < 5_000);
    assert_eq!(flow(&server).to_auth(&refreshed).api_key.as_deref(), Some("LLM|fresh-key"));
    let error = expect_err(flow(&server).refresh(&identity(), CancellationToken::new()).await);
    assert_eq!(error, "Meta did not issue an API key. Complete setup at https://dev.meta.ai/billing");
    let error = expect_err(flow(&server).refresh(&identity(), CancellationToken::new()).await);
    assert_eq!(error, "Meta session expired (status 401). Run `/login meta` to sign in again.: Session invalid");
    let error = expect_err(flow(&server).refresh(&identity(), CancellationToken::new()).await);
    assert_eq!(error, "Meta API key mint failed with status 500: oops");
}

#[tokio::test(start_paused = true)]
async fn device_errors_are_reported() {
    let server = MockServer::start(|request: &Recorded| {
        if request.path.ends_with("/authorization/") {
            Reply::Json(200, json!({ "device_code": "d", "user_code": "u", "verification_uri": "https://m/device" }))
        } else {
            Reply::Json(400, json!({ "error": "access_denied" }))
        }
    })
    .await;
    let interaction = TestInteraction::new(|_, _, _| panic!("no prompts"));
    let result = flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Meta login was denied.");
    let AuthEvent::DeviceCode { interval_seconds, expires_in_seconds, .. } = &interaction.events()[0] else { panic!() };
    assert_eq!((*interval_seconds, *expires_in_seconds), (None, None));

    let failing =
        MockServer::start(|_: &Recorded| Reply::Json(400, json!({ "error_description": "bad client" }))).await;
    let result = flow(&failing).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Meta device authorization failed with status 400: bad client");
}
