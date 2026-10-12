use std::sync::Arc;

use serde_json::{Value, json};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::support::{
    MockServer, Recorded, Reply, TestInteraction, answer, auth_url, expect_err, param, pending, sequence,
};
use crate::auth::oauth::{OpenAICodexOAuth, base64url_encode};
use crate::auth::{AuthError, AuthEvent, AuthPrompt, LoginOptions, OAuthAuth, OAuthCredential};

fn access_token(account_id: &str) -> String {
    let payload = json!({ "https://api.openai.com/auth": { "chatgpt_account_id": account_id } });
    format!("{}.{}.signature", base64url_encode(br#"{"alg":"none"}"#), base64url_encode(payload.to_string().as_bytes()))
}

fn pending_reply() -> (u16, Value) {
    (
        403,
        json!({ "error": { "message": "Device authorization is pending.", "code": "deviceauth_authorization_pending" } }),
    )
}

async fn device_server(polls: Vec<(u16, Value)>) -> MockServer {
    let poll = sequence(polls);
    MockServer::start(move |request: &Recorded| match request.path.as_str() {
        "/auth.openai.com/api/accounts/deviceauth/usercode" => {
            assert_eq!(request.json(), json!({ "client_id": "app_EMoamEEZ73f0CkXaXp7hrann" }));
            Reply::Json(200, json!({ "device_auth_id": "device-auth-id", "user_code": "ABCD-1234", "interval": "5" }))
        }
        "/auth.openai.com/api/accounts/deviceauth/token" => {
            assert_eq!(request.json(), json!({ "device_auth_id": "device-auth-id", "user_code": "ABCD-1234" }));
            poll()
        }
        "/auth.openai.com/oauth/token" => Reply::Json(
            200,
            json!({ "access_token": access_token("account-123"), "refresh_token": "refresh-token", "expires_in": 3600 }),
        ),
        other => panic!("unexpected request {other}"),
    })
    .await
}

fn device_interaction() -> TestInteraction {
    TestInteraction::new(|prompt, _, _| match prompt {
        AuthPrompt::Select { .. } => answer("device_code"),
        other => panic!("unexpected prompt {other:?}"),
    })
}

fn flow(server: &MockServer) -> OpenAICodexOAuth {
    OpenAICodexOAuth { hooks: server.hooks(), callback_port: 0 }
}

#[tokio::test(start_paused = true)]
async fn device_code_login_polls_then_exchanges_the_code() {
    let completed = (200, json!({ "authorization_code": "oauth-code", "code_verifier": "device-code-verifier" }));
    let server = device_server(vec![pending_reply(), completed]).await;
    let interaction = device_interaction();
    let start = Instant::now();
    let credential =
        flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(credential.access, access_token("account-123"));
    assert_eq!(credential.extra_str("accountId"), Some("account-123"));
    let device_events: Vec<_> =
        interaction.events().into_iter().filter(|e| matches!(e, AuthEvent::DeviceCode { .. })).collect();
    assert_eq!(
        device_events,
        [AuthEvent::DeviceCode {
            user_code: "ABCD-1234".into(),
            verification_uri: "https://auth.openai.com/codex/device".into(),
            interval_seconds: Some(5),
            expires_in_seconds: Some(900),
        }]
    );
    let polls: Vec<u128> = server.requests_to("/deviceauth/token").iter().map(|r| (r.at - start).as_millis()).collect();
    assert_eq!(polls, [0, 5000]);
    let exchange = &server.requests_to("/oauth/token")[0];
    assert_eq!(exchange.header("content-type"), Some("application/x-www-form-urlencoded"));
    assert_eq!(exchange.form("grant_type").as_deref(), Some("authorization_code"));
    assert_eq!(exchange.form("code").as_deref(), Some("oauth-code"));
    assert_eq!(exchange.form("redirect_uri").as_deref(), Some("https://auth.openai.com/deviceauth/callback"));
    assert_eq!(exchange.form("code_verifier").as_deref(), Some("device-code-verifier"));
}

#[tokio::test(start_paused = true)]
async fn device_code_login_treats_404_as_pending_and_times_out_after_15_minutes() {
    let server = device_server(vec![(404, json!({})); 181]).await;
    let result = flow(&server).login(&device_interaction(), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Device flow timed out");
    assert_eq!(server.requests_to("/deviceauth/token").len(), 180);
}

#[tokio::test(start_paused = true)]
async fn device_code_failures_include_the_body_and_cancel_stops_polling() {
    let server = device_server(vec![(500, json!({ "error": "boom" }))]).await;
    let result = flow(&server).login(&device_interaction(), &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), r#"OpenAI Codex device auth failed with status 500: {"error":"boom"}"#);

    // The mock cancels the login while answering the second poll.
    let cancel = CancellationToken::new();
    let (stop, polls) = (cancel.clone(), std::sync::atomic::AtomicUsize::new(0));
    let server = MockServer::start(move |request: &Recorded| {
        if request.path.ends_with("/usercode") {
            return Reply::Json(200, json!({ "device_auth_id": "d", "user_code": "U", "interval": 5 }));
        }
        if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
            stop.cancel();
        }
        let (status, body) = pending_reply();
        Reply::Json(status, body)
    })
    .await;
    let result = flow(&server).login(&device_interaction(), &LoginOptions::default(), cancel).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
    assert_eq!(server.requests_to("/deviceauth/token").len(), 2);
}

#[tokio::test]
async fn refresh_reports_failures_without_rewrapping() {
    let server = MockServer::start(|_: &Recorded| Reply::Text(401, "Could not validate your token".into())).await;
    let old = OAuthCredential { refresh: "refresh".into(), ..OAuthCredential::default() };
    let error = expect_err(flow(&server).refresh(&old, CancellationToken::new()).await);
    assert_eq!(error, "OpenAI Codex token refresh failed (401): Could not validate your token");

    let token = access_token("acct");
    let ok = MockServer::start(move |request: &Recorded| {
        assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
        Reply::Json(200, json!({ "access_token": token, "refresh_token": "next", "expires_in": 60 }))
    })
    .await;
    let refreshed = flow(&ok).refresh(&old, CancellationToken::new()).await.unwrap();
    assert_eq!((refreshed.refresh.as_str(), refreshed.extra_str("accountId")), ("next", Some("acct")));
    assert_eq!(flow(&ok).to_auth(&refreshed).api_key, Some(refreshed.access));

    let missing = MockServer::start(|_: &Recorded| Reply::Json(200, json!({ "access_token": "a" }))).await;
    let error = expect_err(flow(&missing).refresh(&old, CancellationToken::new()).await);
    assert_eq!(error, r#"OpenAI Codex token refresh response missing fields: {"access_token":"a"}"#);
}

fn exchange_server() -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    |request: &Recorded| {
        assert_eq!(request.path, "/auth.openai.com/oauth/token");
        Reply::Json(
            200,
            json!({ "access_token": access_token("acct"), "refresh_token": "refresh", "expires_in": 3600 }),
        )
    }
}

#[tokio::test]
async fn browser_login_falls_back_to_the_pasted_redirect_when_the_port_is_taken() {
    let server = MockServer::start(exchange_server()).await;
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flow = OpenAICodexOAuth { hooks: server.hooks(), callback_port: blocker.local_addr().unwrap().port() };
    let interaction = TestInteraction::new(|prompt, cancel, events| match prompt {
        AuthPrompt::Select { .. } => answer("browser"),
        _ => match auth_url(&events) {
            Some(url) => answer(format!(
                "http://localhost:1455/auth/callback?code=pasted-code&state={}",
                param(&url, "state").unwrap()
            )),
            None => pending(cancel),
        },
    });
    let options = LoginOptions { agent_name: Some("my-app".into()), ..LoginOptions::default() };
    let credential = flow.login(&interaction, &options, CancellationToken::new()).await.unwrap();
    assert_eq!(credential.extra_str("accountId"), Some("acct"));
    let url = interaction.auth_url();
    assert_eq!(param(&url, "originator").as_deref(), Some("my-app"));
    assert_eq!(param(&url, "redirect_uri").as_deref(), Some("http://localhost:1455/auth/callback"));
    assert_eq!(param(&url, "codex_cli_simplified_flow").as_deref(), Some("true"));
    assert_eq!(param(&url, "state").map(|state| state.len()), Some(32));
    let exchange = &server.requests()[0];
    assert_eq!(exchange.form("code").as_deref(), Some("pasted-code"));
    assert_eq!(exchange.form("redirect_uri").as_deref(), Some("http://localhost:1455/auth/callback"));
}

#[tokio::test]
async fn browser_login_accepts_the_loopback_callback_and_rejects_pasted_state_mismatch() {
    let server = MockServer::start(exchange_server()).await;
    let port = super::support::free_port().await;
    let flow = Arc::new(OpenAICodexOAuth { hooks: server.hooks(), callback_port: port });
    let interaction = TestInteraction::new(|prompt, cancel, _| match prompt {
        AuthPrompt::Select { .. } => answer("browser"),
        _ => pending(cancel),
    })
    .on_notify(move |event| {
        let AuthEvent::AuthUrl { url, .. } = event else { return };
        let state = param(&reqwest::Url::parse(url).unwrap(), "state").unwrap();
        let target = format!("http://127.0.0.1:{port}/auth/callback?code=browser-code&state={state}");
        tokio::spawn(async move { super::support::get(&target).await });
    });
    let credential = flow.login(&interaction, &LoginOptions::default(), CancellationToken::new()).await.unwrap();
    assert_eq!(credential.refresh, "refresh");
    assert_eq!(param(&interaction.auth_url(), "originator").as_deref(), Some("pi"));
    assert_eq!(server.requests()[0].form("code").as_deref(), Some("browser-code"));

    let mismatch = TestInteraction::new(|prompt, _, _| match prompt {
        AuthPrompt::Select { .. } => answer("browser"),
        _ => answer("http://localhost:1455/auth/callback?code=c&state=wrong"),
    });
    let result = flow.login(&mismatch, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "State mismatch");
}
