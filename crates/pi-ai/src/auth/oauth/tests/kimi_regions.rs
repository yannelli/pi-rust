use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction};
use crate::auth::oauth::KimiCodingOAuth;
use crate::auth::{AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

fn token() -> Value {
    json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600})
}

#[tokio::test(start_paused = true)]
async fn kimi_login_and_refresh_keep_the_selected_region() {
    for (override_host, domain) in [(None, "kimi.ai"), (Some("https://auth.kimi.com/"), "kimi.com")] {
        let server = MockServer::start(move |request: &Recorded| {
            assert!(request.path.starts_with(&format!("/auth.{domain}/api/oauth/")));
            if request.path.ends_with("/device_authorization") {
                Reply::Json(
                    200,
                    json!({"device_code": "device", "user_code": "ABCD", "interval": 1,
                    "verification_uri": format!("https://www.{domain}/code"),
                    "verification_uri_complete": format!("https://www.{domain}/code?user_code=ABCD")}),
                )
            } else {
                Reply::Json(200, token())
            }
        })
        .await;
        let mut hooks = server.hooks();
        hooks.env =
            Some(Arc::new(
                move |key| {
                    if key == "KIMI_CODE_OAUTH_HOST" { override_host.map(str::to_string) } else { None }
                },
            ));
        let flow = KimiCodingOAuth { hooks };
        let interaction = TestInteraction::new(|_, _, _| panic!("no prompts"));
        let credential = flow.login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
        let saved: OAuthCredential = serde_json::from_value(serde_json::to_value(&credential).unwrap()).unwrap();
        assert_eq!(saved.extra["oauthHost"], format!("https://auth.{domain}"));
        assert!(matches!(&interaction.events()[0], AuthEvent::DeviceCode { verification_uri, .. }
            if verification_uri == &format!("https://www.{domain}/code?user_code=ABCD")));

        let fresh_process = KimiCodingOAuth { hooks: server.hooks() };
        let refreshed = fresh_process.refresh(&saved, CancellationToken::new()).await.unwrap();
        assert_eq!(refreshed.extra, saved.extra);
        let auth = fresh_process.to_auth(&refreshed);
        assert_eq!(auth.base_url, Some(format!("https://api.{domain}/coding/v1")));
        assert_eq!(auth.headers["Authorization"], "Bearer new-access");
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[2].form("grant_type").as_deref(), Some("refresh_token"));
        assert_eq!(requests[2].form("refresh_token").as_deref(), Some("new-refresh"));
    }
}

#[tokio::test(start_paused = true)]
async fn kimi_custom_endpoints_are_retained_and_legacy_credentials_stay_mainland() {
    let server = MockServer::start(|request: &Recorded| {
        assert_eq!(request.path, "/custom.example/api/oauth/token");
        Reply::Json(200, token())
    })
    .await;
    let mut hooks = server.hooks();
    hooks.env = Some(Arc::new(|key| match key {
        "KIMI_OAUTH_HOST" => Some("https://custom.example/".into()),
        "KIMI_CODE_BASE_URL" => Some("https://api.custom.example/v1/".into()),
        _ => None,
    }));
    let flow = KimiCodingOAuth { hooks };
    let legacy = OAuthCredential { access: "old".into(), refresh: "refresh".into(), expires: 0, ..Default::default() };
    let mut saved_global = legacy.clone();
    saved_global.extra = json!({"oauthHost": "https://auth.kimi.ai", "baseUrl": "https://api.kimi.ai/coding/v1"})
        .as_object()
        .unwrap()
        .clone();
    let refreshed = flow.refresh(&saved_global, CancellationToken::new()).await.unwrap();
    let restarted = KimiCodingOAuth { hooks: server.hooks() };
    assert_eq!(restarted.to_auth(&refreshed).base_url.as_deref(), Some("https://api.custom.example/v1"));
    assert_eq!(refreshed.extra["oauthHost"], "https://custom.example");
    assert_eq!(restarted.to_auth(&legacy).base_url.as_deref(), Some("https://api.kimi.com/coding/v1"));

    let mut hooks = server.hooks();
    hooks.env = Some(Arc::new(|key| (key == "KIMI_OAUTH_HOST").then(|| "https://custom.example".into())));
    assert_eq!(KimiCodingOAuth { hooks }.to_auth(&legacy).base_url, None);
}
