use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, answer, auth_url, expect_err, get, param, pending};
use crate::auth::oauth::AnthropicOAuth;
use crate::auth::{AuthError, AuthEvent, AuthPrompt, LoginOptions, OAuthAuth, OAuthCredential};

const TOKEN_PATH: &str = "/platform.claude.com/v1/oauth/token";

async fn token_server() -> MockServer {
    MockServer::start(|request: &Recorded| {
        assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", TOKEN_PATH));
        assert_eq!(request.header("content-type"), Some("application/json"));
        Reply::Json(200, json!({ "access_token": "access", "refresh_token": "refresh", "expires_in": 3600 }))
    })
    .await
}

fn flow(server: &MockServer, port: u16) -> AnthropicOAuth {
    AnthropicOAuth { hooks: server.hooks(), callback_port: port }
}

fn select_then(
    method: &'static str,
    manual: impl Fn(&reqwest::Url) -> String + Send + Sync + 'static,
) -> TestInteraction {
    TestInteraction::new(move |prompt, cancel, events| match prompt {
        AuthPrompt::Select { .. } => answer(method),
        AuthPrompt::ManualCode { .. } => match auth_url(&events) {
            Some(url) => answer(manual(&url)),
            None => pending(cancel),
        },
        other => panic!("unexpected prompt {other:?}"),
    })
}

#[tokio::test]
async fn pasted_redirect_url_exchanges_with_the_loopback_redirect_uri() {
    let server = token_server().await;
    let interaction = select_then("browser", |url| {
        let redirect = param(url, "redirect_uri").unwrap();
        format!("{redirect}?code=manual-code&state={}", param(url, "state").unwrap())
    });
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = blocker.local_addr().unwrap().port();
    drop(blocker);
    let credential = flow(&server, port).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    let credential = credential.unwrap();
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), ("access", "refresh"));
    let now = crate::auth::oauth::http::now_ms();
    assert!(credential.expires > now + 3_000_000 && credential.expires <= now + 3_300_000);

    let url = interaction.auth_url();
    assert_eq!((url.host_str(), url.path()), (Some("claude.ai"), "/oauth/authorize"));
    assert_eq!(param(&url, "client_id").as_deref(), Some("9d1c250a-e61b-44d9-88ed-5944d1962f5e"));
    assert_eq!(param(&url, "code").as_deref(), Some("true"));
    assert_eq!(param(&url, "code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(
        param(&url, "scope").as_deref(),
        Some(
            "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload"
        )
    );
    let body = server.requests()[0].json();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "manual-code");
    assert_eq!(body["redirect_uri"], format!("http://localhost:{port}/callback"));
    assert_eq!(body["state"], body["code_verifier"]);
    assert_eq!(param(&url, "state").as_deref(), body["code_verifier"].as_str());
    let prompts = interaction.prompts.lock().unwrap();
    assert!(prompts.iter().all(|(_, token)| token.is_cancelled()), "prompts are dismissed after login");
}

#[tokio::test]
async fn copy_code_login_uses_the_platform_redirect() {
    let server = token_server().await;
    let interaction = select_then("copy_code", |url| format!("copied-code#{}", param(url, "state").unwrap()));
    let credential = flow(&server, 0).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(credential.unwrap().access, "access");
    let url = interaction.auth_url();
    assert_eq!(param(&url, "redirect_uri").as_deref(), Some("https://platform.claude.com/oauth/code/callback"));
    let body = server.requests()[0].json();
    assert_eq!((body["code"].as_str(), body["state"].as_str()), (Some("copied-code"), param(&url, "state").as_deref()));
    assert_eq!(body["redirect_uri"], "https://platform.claude.com/oauth/code/callback");
    let prompts = interaction.prompts.lock().unwrap();
    let AuthPrompt::Select { message, options } = &prompts[0].0 else { panic!("select first") };
    assert_eq!(message, "Select Anthropic login method:");
    let labels: Vec<_> = options.iter().map(|option| (option.id.as_str(), option.label.as_str())).collect();
    assert_eq!(labels, [("browser", "Browser login (default)"), ("copy_code", "Copy code login (headless)")]);
    assert!(matches!(&prompts[1].0, AuthPrompt::ManualCode { placeholder: Some(p), .. } if p == "code#state"));
}

#[tokio::test]
async fn pasted_state_mismatch_and_cancelled_selection_fail() {
    let server = token_server().await;
    let interaction = select_then("copy_code", |_| "code#wrong-state".into());
    let result = flow(&server, 0).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "OAuth state mismatch");

    let cancelled = TestInteraction::new(|_, _, _| Box::pin(async { Err(AuthError::Cancelled) }));
    let result = flow(&server, 0).login(&cancelled, &LoginOptions::default(), CancellationToken::new()).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
    let unknown = select_then("other", |_| String::new());
    let result = flow(&server, 0).login(&unknown, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Unknown Anthropic login method: other");
    assert!(server.requests().is_empty());
}

/// Completes the browser login by hitting the loopback callback from the auth URL.
async fn login_through_browser(port: u16) -> (OAuthCredential, String, (u16, String), MockServer) {
    let server = token_server().await;
    let (page_sender, page) = tokio::sync::oneshot::channel();
    let page_sender = std::sync::Mutex::new(Some(page_sender));
    let interaction = TestInteraction::new(|prompt, cancel, _| match prompt {
        AuthPrompt::Select { .. } => answer("browser"),
        _ => pending(cancel),
    })
    .on_notify(move |event| {
        let AuthEvent::AuthUrl { url, .. } = event else { return };
        let url = reqwest::Url::parse(url).unwrap();
        let redirect = param(&url, "redirect_uri").unwrap().replace("localhost", "127.0.0.1");
        let target = format!("{redirect}?code=browser-code&state={}", param(&url, "state").unwrap());
        let sender = page_sender.lock().unwrap().take().unwrap();
        tokio::spawn(async move { sender.send(get(&target).await.unwrap()) });
    });
    let credential = flow(&server, port).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    let redirect = param(&interaction.auth_url(), "redirect_uri").unwrap();
    (credential.unwrap(), redirect, page.await.unwrap(), server)
}

#[tokio::test]
async fn browser_callback_completes_login_and_shows_the_page() {
    let (credential, redirect, (status, body), server) = login_through_browser(0).await;
    assert_eq!(credential.access, "access");
    assert_eq!((status, body.contains("Signed in to Anthropic.")), (200, true));
    let request = server.requests()[0].json();
    assert_eq!(
        (request["code"].as_str(), request["redirect_uri"].as_str()),
        (Some("browser-code"), Some(redirect.as_str()))
    );
}

#[tokio::test]
async fn browser_login_falls_back_to_a_free_port_when_the_preferred_one_is_taken() {
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = blocker.local_addr().unwrap().port();
    let (credential, redirect, (status, _), server) = login_through_browser(taken).await;
    let redirect_url = reqwest::Url::parse(&redirect).unwrap();
    assert_eq!((redirect_url.host_str(), redirect_url.path()), (Some("localhost"), "/callback"));
    assert_ne!(redirect_url.port(), Some(taken));
    assert_eq!((credential.access.as_str(), status), ("access", 200));
    assert_eq!(server.requests()[0].json()["redirect_uri"], redirect);
}

#[tokio::test]
async fn refresh_omits_scope_and_reports_http_failures() {
    let server = token_server().await;
    let old = OAuthCredential { refresh: "refresh-token".into(), ..OAuthCredential::default() };
    let refreshed = flow(&server, 0).refresh(&old, CancellationToken::new()).await.unwrap();
    assert_eq!((refreshed.access.as_str(), refreshed.refresh.as_str()), ("access", "refresh"));
    let body = server.requests()[0].json();
    assert_eq!(body["grant_type"], "refresh_token");
    assert_eq!(body["refresh_token"], "refresh-token");
    assert!(body.get("scope").is_none() && body["client_id"].is_string());
    assert_eq!(flow(&server, 0).to_auth(&refreshed).api_key.as_deref(), Some("access"));

    let failing = MockServer::start(|_: &Recorded| Reply::Json(400, json!({ "error": "invalid_grant" }))).await;
    let error = expect_err(flow(&failing, 0).refresh(&old, CancellationToken::new()).await);
    assert!(
        error.starts_with("Anthropic token refresh request failed. url=https://platform.claude.com/v1/oauth/token;")
    );
    assert!(error.contains(r#"status=400"#) && error.contains("invalid_grant"), "{error}");
}
