use std::sync::Arc;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::support::{MockServer, Recorded, Reply, TestInteraction, answer, auth_url, expect_err, get, param, pending};
use crate::auth::oauth::OpenAIChatGPTOAuth;
use crate::auth::{AuthError, AuthEvent, AuthPrompt, LoginOptions, OAuthAuth, OAuthCredential};

const REQUIRED_SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const DEVICE_ID: &str = "E61BBE28-07ef-466d-8e5d-a344f94ab305";

fn token_response() -> Value {
    json!({ "access_token": "access-token", "refresh_token": "refresh-token", "expires_in": 3600,
        "id_token": "id-token", "scope": REQUIRED_SCOPE })
}

async fn token_server(response: Value) -> MockServer {
    MockServer::start(move |request: &Recorded| {
        assert_eq!(request.path, "/auth.openai.com/api/accounts/oauth/token");
        assert_eq!(request.header("accept"), Some("application/json"));
        Reply::Json(200, response.clone())
    })
    .await
}

fn options() -> LoginOptions {
    LoginOptions { device_id: Some(Arc::new(|| DEVICE_ID.to_string())), agent_name: None }
}

/// Answers the manual prompt with the redirect URL the browser would land on.
fn paste_callback(client_id: Option<&'static str>) -> TestInteraction {
    TestInteraction::new(move |prompt, cancel, events| {
        let AuthPrompt::ManualCode { .. } = prompt else { panic!("unexpected prompt {prompt:?}") };
        let Some(url) = auth_url(&events) else { return pending(cancel) };
        let mut callback = reqwest::Url::parse(&param(&url, "redirect_uri").unwrap()).unwrap();
        callback
            .query_pairs_mut()
            .append_pair("code", "authorization-code")
            .append_pair("state", &param(&url, "state").unwrap());
        if let Some(client_id) = client_id {
            callback.query_pairs_mut().append_pair("client_id", client_id);
        }
        answer(callback.to_string())
    })
}

fn flow(server: &MockServer) -> OpenAIChatGPTOAuth {
    OpenAIChatGPTOAuth { hooks: server.hooks(), callback_port: 0 }
}

#[tokio::test]
async fn login_registers_a_client_and_stores_its_issued_id_and_scopes() {
    let server = token_server(token_response()).await;
    let flow = flow(&server);
    let interaction = paste_callback(Some("oaiapp_issued"));
    let credential = flow.login(&interaction, &options(), CancellationToken::new()).await.unwrap();

    let url = interaction.auth_url();
    let redirect_uri = param(&url, "redirect_uri").unwrap();
    let port = reqwest::Url::parse(&redirect_uri).unwrap().port().unwrap();
    assert_ne!(port, 0);
    assert_eq!(url.as_str().split('?').next(), Some("https://auth.openai.com/api/accounts/authorize"));
    let expected = [
        ("client_id", "dynamic_agent_client".to_string()),
        ("agent_name_hint", "Pi".into()),
        ("ext_agent_host_id", format!("urn:uuid:{}", DEVICE_ID.to_lowercase())),
        ("scope", REQUIRED_SCOPE.into()),
        ("redirect_uri", format!("http://127.0.0.1:{port}/auth/callback")),
        ("resource", "https://api.openai.com/v1".into()),
        ("code_challenge_method", "S256".into()),
    ];
    for (name, value) in expected {
        assert_eq!(param(&url, name).as_deref(), Some(value.as_str()), "{name}");
    }
    assert!(param(&url, "nonce").is_some_and(|nonce| nonce.len() == 43));
    let exchange = &server.requests()[0];
    assert_eq!(exchange.form("client_id").as_deref(), Some("oaiapp_issued"));
    assert_eq!(exchange.form("code").as_deref(), Some("authorization-code"));
    assert_eq!(exchange.form("redirect_uri").as_deref(), Some(redirect_uri.as_str()));
    assert_eq!(exchange.form("resource").as_deref(), Some("https://api.openai.com/v1"));
    assert!(exchange.form("code_verifier").is_some_and(|verifier| !verifier.is_empty()));
    assert_eq!((credential.access.as_str(), credential.refresh.as_str()), ("access-token", "refresh-token"));
    assert_eq!(credential.extra_str("clientId"), Some("oaiapp_issued"));
    let scopes: Vec<&str> = REQUIRED_SCOPE.split(' ').collect();
    assert_eq!(credential.extra["scopes"], json!(scopes));
    assert!(interaction.prompts.lock().unwrap()[0].1.is_cancelled());
}

#[tokio::test]
async fn login_uses_the_agent_name_and_accepts_the_browser_callback() {
    let server = token_server(token_response()).await;
    let flow = flow(&server);
    let (page_sender, page) = tokio::sync::oneshot::channel();
    let page_sender = std::sync::Mutex::new(Some(page_sender));
    let interaction = TestInteraction::new(|_, cancel, _| pending(cancel)).on_notify(move |event| {
        let AuthEvent::AuthUrl { url, .. } = event else { return };
        let url = reqwest::Url::parse(url).unwrap();
        let state = param(&url, "state").unwrap();
        let base = param(&url, "redirect_uri").unwrap();
        let sender = page_sender.lock().unwrap().take().unwrap();
        tokio::spawn(async move {
            let stray = get(&format!("{base}?code=c&state=wrong&client_id=x")).await.unwrap();
            let page = get(&format!("{base}?code=c&state={state}&client_id=oaiapp_cb")).await.unwrap();
            sender.send((stray, page))
        });
    });
    let options = LoginOptions { agent_name: Some("my-app".into()), ..options() };
    let credential = flow.login(&interaction, &options, CancellationToken::new()).await.unwrap();
    assert_eq!(credential.extra_str("clientId"), Some("oaiapp_cb"));
    assert_eq!(param(&interaction.auth_url(), "agent_name_hint").as_deref(), Some("my-app"));
    let ((stray_status, stray_body), (status, body)) = page.await.unwrap();
    assert_eq!((stray_status, stray_body.contains("OAuth state mismatch")), (400, true));
    assert_eq!((status, body.contains("ChatGPT authentication completed.")), (200, true));
}

#[tokio::test]
async fn login_rejects_missing_client_ids_scopes_device_ids_and_busy_ports() {
    let server = token_server(token_response()).await;
    let result = flow(&server).login(&paste_callback(None), &options(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "OpenAI OAuth registration callback did not contain an issued client ID");

    let narrow = token_server(json!({ "access_token": "a", "refresh_token": "r", "expires_in": 60,
        "id_token": "i", "scope": "openid" }))
    .await;
    let result = flow(&narrow).login(&paste_callback(Some("id")), &options(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "OpenAI OAuth grant did not include chatgpt.tokens.use.direct");

    let interaction = paste_callback(Some("id"));
    let result = flow(&server).login(&interaction, &LoginOptions::default(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "Sign in with ChatGPT requires a device ID (UUID) for this installation");
    let bad_id = LoginOptions { device_id: Some(Arc::new(|| "not-a-uuid".into())), agent_name: None };
    let result = flow(&server).login(&interaction, &bad_id, CancellationToken::new()).await;
    assert!(expect_err(result).contains("requires a device ID"));

    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = blocker.local_addr().unwrap().port();
    let busy = OpenAIChatGPTOAuth { hooks: server.hooks(), callback_port: port };
    let result = busy.login(&interaction, &options(), CancellationToken::new()).await;
    assert!(expect_err(result).starts_with(&format!("Port {port} is in use")));

    let wrong_url = TestInteraction::new(|_, _, _| answer("http://localhost:1/elsewhere?code=c"));
    let result = flow(&server).login(&wrong_url, &options(), CancellationToken::new()).await;
    assert!(expect_err(result).starts_with("The pasted callback URL must start with http://127.0.0.1:"));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = flow(&server).login(&paste_callback(Some("id")), &options(), cancel).await;
    assert!(matches!(result, Err(AuthError::Cancelled)));
}

fn connected() -> OAuthCredential {
    let mut credential = OAuthCredential { access: "old".into(), refresh: "old-refresh".into(), ..Default::default() };
    credential.extra.insert("clientId".into(), json!("oaiapp_existing"));
    credential
}

#[tokio::test]
async fn refresh_uses_the_issued_client_id_and_requires_rotation() {
    let mut response = token_response();
    response["access_token"] = json!("new-access");
    let server = token_server(response).await;
    let before = crate::auth::oauth::http::now_ms();
    let credential = flow(&server).refresh(&connected(), CancellationToken::new()).await.unwrap();
    assert!(credential.expires >= before + (3600 - 180) * 1000);
    assert!(credential.expires <= crate::auth::oauth::http::now_ms() + (3600 - 180) * 1000);
    let request = &server.requests()[0];
    assert_eq!(request.form("grant_type").as_deref(), Some("refresh_token"));
    assert_eq!(request.form("client_id").as_deref(), Some("oaiapp_existing"));
    assert_eq!(request.form("refresh_token").as_deref(), Some("old-refresh"));
    assert_eq!(request.form("resource").as_deref(), Some("https://api.openai.com/v1"));
    assert!(request.form("scope").is_none());
    assert_eq!((credential.access.as_str(), credential.extra_str("clientId")), ("new-access", Some("oaiapp_existing")));

    let mut unrotated = token_response();
    unrotated.as_object_mut().unwrap().remove("refresh_token");
    let server = token_server(unrotated).await;
    let result = flow(&server).refresh(&connected(), CancellationToken::new()).await;
    assert_eq!(expect_err(result), "OpenAI OAuth token response has invalid refresh_token");
    let result = flow(&server).refresh(&OAuthCredential::default(), CancellationToken::new()).await;
    assert!(expect_err(result).starts_with("Stored OpenAI OAuth credential does not contain an issued client ID"));
}
