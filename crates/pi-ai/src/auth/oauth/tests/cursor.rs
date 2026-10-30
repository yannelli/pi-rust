use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, expect_err, param, sequence};
use crate::auth::oauth::{CursorOAuth, base64url_encode};
use crate::auth::{AuthEvent, LoginOptions, OAuthAuth, OAuthCredential};

fn jwt(exp: i64) -> String {
    format!("h.{}.s", base64url_encode(json!({ "sub": "github|1", "exp": exp }).to_string().as_bytes()))
}

fn flow(server: &MockServer) -> CursorOAuth {
    CursorOAuth { hooks: server.hooks() }
}

#[tokio::test(start_paused = true)]
async fn login_polls_with_the_verifier_of_the_browser_challenge() {
    let access = jwt(2_000_000_000);
    let polls = sequence(vec![
        (404, json!({})),
        (404, json!({})),
        (200, json!({ "accessToken": access, "refreshToken": "refresh-1" })),
    ]);
    let server = MockServer::start(move |request: &Recorded| {
        assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/api2.cursor.sh/auth/poll"));
        polls()
    })
    .await;
    let interaction = TestInteraction::new(|prompt, _, _| panic!("unexpected prompt {prompt:?}"));
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), (access.as_str(), "refresh-1"));
    assert_eq!(credential.expires, 2_000_000_000_000 - 300_000);

    let url = interaction.auth_url();
    assert_eq!(url.as_str().split('?').next(), Some("https://cursor.com/loginDeepControl"));
    assert_eq!(
        (param(&url, "mode").as_deref(), param(&url, "redirectTarget").as_deref()),
        (Some("login"), Some("cli"))
    );
    assert_eq!(param(&url, "uuid").map(|uuid| uuid.len()), Some(36));
    assert_eq!(param(&url, "challenge").map(|challenge| challenge.len()), Some(43));
    assert_eq!(server.requests().len(), 3);
    assert!(interaction.events().iter().any(|event| matches!(event, AuthEvent::Progress { .. })));
}

#[tokio::test(start_paused = true)]
async fn login_fails_after_three_consecutive_errors() {
    let server = MockServer::start(|_: &Recorded| Reply::Json(500, json!({}))).await;
    let interaction = TestInteraction::new(|prompt, _, _| panic!("unexpected prompt {prompt:?}"));
    let result = flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Too many consecutive errors during Cursor auth polling");
    assert_eq!(server.requests().len(), 3);
}

fn stored() -> OAuthCredential {
    OAuthCredential { access: "old".into(), refresh: "refresh-1".into(), expires: 0, ..OAuthCredential::default() }
}

#[tokio::test]
async fn refresh_keeps_the_refresh_token_unless_rotated_and_honors_should_logout() {
    let access = jwt(2_000_000_000);
    let replies = sequence(vec![
        (200, json!({ "access_token": access })),
        (200, json!({ "access_token": access, "refresh_token": "refresh-2" })),
        (200, json!({ "access_token": "", "shouldLogout": true })),
    ]);
    let server = MockServer::start(move |request: &Recorded| {
        assert_eq!(request.path, "/api2.cursor.sh/oauth/token");
        let body: Value = request.json();
        assert_eq!(body["client_id"], "KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB");
        assert_eq!(body["grant_type"], "refresh_token");
        replies()
    })
    .await;
    let oauth = flow(&server);
    let kept = oauth.refresh(&stored(), CancellationToken::new()).await.unwrap();
    assert_eq!((kept.access.as_str(), kept.refresh.as_str()), (access.as_str(), "refresh-1"));
    let rotated = oauth.refresh(&stored(), CancellationToken::new()).await.unwrap();
    assert_eq!(rotated.refresh, "refresh-2");
    let ended = oauth.refresh(&stored(), CancellationToken::new()).await;
    assert_eq!(expect_err(ended), "invalid_grant: Cursor ended this session; run /login cursor again");
    assert_eq!(oauth.to_auth(&kept).api_key.as_deref(), Some(access.as_str()));
}
