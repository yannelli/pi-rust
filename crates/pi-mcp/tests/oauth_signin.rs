//! Signing in to MCP servers: issuer checks, scope step-up, callback settings, and Client ID
//! Metadata Documents. Ported from coding-agent's mcp-oauth-refresh.test.ts and
//! agent-session-mcp-oauth.test.ts.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::oauth_server::{Browser, BrowserPrompt, OAuthMcpServer, OAuthServerOptions, start_oauth_mcp_server};
use pi_mcp::OAuthError;
use pi_mcp::oauth::pkce::{base64_url, sha256};
use pi_mcp::oauth::{LoopbackCallbackFactory, McpOAuthStateStore, OAuthChallenge};
use pi_mcp::runtime::{
    InMemoryAuthStorageBackend, McpOAuthCredentialStore, McpOAuthServerStore, McpOAuthSettings, McpSignInPrompt,
    SignInOptions, sign_in_mcp_server,
};
use tokio_util::sync::CancellationToken;
use url::Url;

fn store_for(server: &OAuthMcpServer) -> Arc<McpOAuthServerStore> {
    let credentials = McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None);
    credentials.for_server("test", &server.url)
}

async fn sign_in(
    server: &OAuthMcpServer,
    store: Arc<dyn McpOAuthStateStore>,
    settings: McpOAuthSettings,
    prompt: Arc<dyn McpSignInPrompt>,
    challenge: Option<OAuthChallenge>,
) -> Result<(), OAuthError> {
    sign_in_mcp_server(SignInOptions {
        server_url: Url::parse(&server.url).unwrap(),
        store,
        settings,
        challenge,
        prompt,
        cancel: CancellationToken::new(),
        callback_factory: Arc::new(LoopbackCallbackFactory),
        client: reqwest::Client::new(),
    })
    .await
}

async fn follow(server: &OAuthMcpServer, settings: McpOAuthSettings) -> (Result<(), OAuthError>, Arc<BrowserPrompt>) {
    let prompt = BrowserPrompt::new(Browser::Follow);
    let result = sign_in(server, store_for(server), settings, prompt.clone(), None).await;
    (result, prompt)
}

fn cimd() -> McpOAuthSettings {
    McpOAuthSettings { client_registration: Some("cimd".into()), ..McpOAuthSettings::default() }
}

#[tokio::test]
async fn rejects_an_authorization_response_from_another_issuer() {
    let options = OAuthServerOptions { iss: Some("https://attacker.example".into()), ..Default::default() };
    let server = start_oauth_mcp_server(options).await;
    let (result, _) = follow(&server, McpOAuthSettings::default()).await;
    assert!(matches!(result, Err(OAuthError::IssuerMismatch { .. })), "{result:?}");
}

#[tokio::test]
async fn keeps_the_granted_scope_when_the_server_asks_for_more() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let store = store_for(&server);
    let prompt = BrowserPrompt::new(Browser::Follow);
    let read = OAuthChallenge { scope: Some("issues:read".into()), ..OAuthChallenge::default() };
    sign_in(&server, store.clone(), McpOAuthSettings::default(), prompt.clone(), Some(read)).await.unwrap();
    // The token response names no scope, so the grant has the requested one.
    let scope = |state: Option<pi_mcp::oauth::McpOAuthState>| state.and_then(|state| state.tokens?.scope);
    assert_eq!(scope(store.load().await.unwrap()).as_deref(), Some("issues:read"));
    // The step-up challenge lists only the missing scope. Requesting just that would lose issues:read.
    let write = OAuthChallenge {
        error: Some("insufficient_scope".into()),
        scope: Some("issues:write".into()),
        ..OAuthChallenge::default()
    };
    sign_in(&server, store.clone(), McpOAuthSettings::default(), prompt.clone(), Some(write)).await.unwrap();
    let scopes = [prompt.opened_param(0, "scope"), prompt.opened_param(1, "scope")];
    assert_eq!(scopes, [Some("issues:read".to_string()), Some("issues:read issues:write".to_string())]);
    assert_eq!(scope(store.load().await.unwrap()).as_deref(), Some("issues:read issues:write"));
}

#[tokio::test]
async fn uses_the_configured_authorization_server_metadata_url() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let url = Url::parse(&format!("{}/missing", server.origin)).unwrap();
    let settings = McpOAuthSettings { auth_server_metadata_url: Some(url), ..McpOAuthSettings::default() };
    let error = follow(&server, settings).await.0.unwrap_err().to_string();
    assert!(error.contains("HTTP 404 loading authorization server metadata"), "{error}");
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[tokio::test]
async fn uses_the_configured_callback_url_and_scope() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let callback_url = format!("http://localhost:{}/callback", free_port());
    let settings = McpOAuthSettings {
        callback_url: Some(callback_url.clone()),
        scope: Some("issues:read".into()),
        ..McpOAuthSettings::default()
    };
    let (result, prompt) = follow(&server, settings).await;
    result.unwrap();
    assert_eq!(prompt.opened_param(0, "redirect_uri"), Some(callback_url));
    assert_eq!(prompt.opened_param(0, "scope").as_deref(), Some("issues:read"));
    assert_eq!(server.log(), ["register", "token code"]);
}

#[tokio::test]
async fn registers_with_the_configured_client_name() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let settings = McpOAuthSettings { client_name: Some("Claude Code".into()), ..McpOAuthSettings::default() };
    follow(&server, settings).await.0.unwrap();
    assert_eq!(server.state().registrations[0]["client_name"], "Claude Code");

    let fallback = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    follow(&fallback, McpOAuthSettings::default()).await.0.unwrap();
    let registration = fallback.state().registrations[0].clone();
    assert_eq!(registration["client_name"], "pi");
    // OpenID Connect servers reject the loopback redirect URI of a `web` client.
    assert_eq!(registration["application_type"], "native");
}

#[tokio::test]
async fn adds_the_listening_port_to_a_callback_url_without_one() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let callback_url = Some("http://127.0.0.1/oauth/done".to_string());
    let settings = McpOAuthSettings { callback_url: callback_url.clone(), ..McpOAuthSettings::default() };
    let (result, prompt) = follow(&server, settings).await;
    result.unwrap();
    let redirect = Url::parse(&prompt.opened_param(0, "redirect_uri").unwrap()).unwrap();
    assert_eq!((redirect.host_str(), redirect.path()), (Some("127.0.0.1"), "/oauth/done"));
    assert!(redirect.port().is_some());

    let port = free_port();
    let fixed = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let settings = McpOAuthSettings { callback_url, callback_port: Some(port), ..McpOAuthSettings::default() };
    let (result, prompt) = follow(&fixed, settings).await;
    result.unwrap();
    assert_eq!(prompt.opened_param(0, "redirect_uri"), Some(format!("http://127.0.0.1:{port}/oauth/done")));
}

#[tokio::test]
async fn registers_dynamically_by_default_even_when_the_server_supports_documents() {
    let server =
        start_oauth_mcp_server(OAuthServerOptions { cimd: true, iss_parameter: true, ..Default::default() }).await;
    follow(&server, McpOAuthSettings::default()).await.0.unwrap();
    assert_eq!(server.state().registrations.len(), 1);
    assert_eq!(server.authorization_param(0, "client_id").as_deref(), Some("client-1"));
}

const DOCUMENT: &str = "https://pi.dev/oauth/client.json";

#[tokio::test]
async fn uses_pis_document_when_authorization_responses_name_their_issuer() {
    let server =
        start_oauth_mcp_server(OAuthServerOptions { cimd: true, iss_parameter: true, ..Default::default() }).await;
    let store = store_for(&server);
    let prompt = BrowserPrompt::new(Browser::Follow);
    sign_in(&server, store.clone(), cimd(), prompt.clone(), None).await.unwrap();
    assert_eq!(server.authorization_param(0, "client_id").as_deref(), Some(DOCUMENT));
    let redirect_uri = server.authorization_param(0, "redirect_uri").unwrap();
    let redirect = Url::parse(&redirect_uri).unwrap();
    assert_eq!((redirect.host_str(), redirect.path()), (Some("127.0.0.1"), "/callback"));
    assert!(redirect.port().is_some());
    {
        let state = server.state();
        assert!(state.registrations.is_empty());
        assert_eq!(state.token_requests[0]["client_id"], DOCUMENT);
        assert_eq!(state.token_requests[0]["redirect_uri"], redirect_uri);
    }
    // The document is not stored, so signing in again refreshes the tokens instead of discarding them.
    assert_eq!(store.load().await.unwrap().unwrap().client_information, None);
    sign_in(&server, store, cimd(), prompt, None).await.unwrap();
    let state = server.state();
    assert_eq!(state.authorizations.len(), 1);
    assert_eq!(state.token_requests[1]["grant_type"], "refresh_token");
    assert_eq!(state.token_requests[1]["client_id"], DOCUMENT);
}

#[tokio::test]
async fn uses_a_document_and_callback_path_specific_to_the_mcp_server_without_the_iss_parameter() {
    let server = start_oauth_mcp_server(OAuthServerOptions { cimd: true, ..Default::default() }).await;
    follow(&server, cimd()).await.0.unwrap();
    // Computed like Codex: the first 9 bytes of the SHA-256 of the MCP server URL.
    let id = base64_url(&sha256(server.url.as_bytes())[..9]);
    assert_eq!(server.authorization_param(0, "client_id"), Some(format!("https://pi.dev/oauth/{id}/client.json")));
    let redirect_uri = server.authorization_param(0, "redirect_uri").unwrap();
    assert_eq!(Url::parse(&redirect_uri).unwrap().path(), format!("/callback/{id}"));
    let state = server.state();
    assert!(state.registrations.is_empty());
    assert_eq!(state.token_requests[0]["redirect_uri"], redirect_uri);
}

#[tokio::test]
async fn replaces_a_registered_client_when_switching_to_the_document() {
    let server =
        start_oauth_mcp_server(OAuthServerOptions { cimd: true, iss_parameter: true, ..Default::default() }).await;
    let store = store_for(&server);
    let prompt = BrowserPrompt::new(Browser::Follow);
    sign_in(&server, store.clone(), McpOAuthSettings::default(), prompt.clone(), None).await.unwrap();
    let client_id = store.load().await.unwrap().unwrap().client_information.map(|client| client.client_id);
    assert_eq!(client_id.as_deref(), Some("client-1"));
    sign_in(&server, store.clone(), cimd(), prompt, None).await.unwrap();
    // The registered client's tokens are not refreshed with another client.
    let client_ids = [server.authorization_param(0, "client_id"), server.authorization_param(1, "client_id")];
    assert_eq!(client_ids, [Some("client-1".to_string()), Some(DOCUMENT.to_string())]);
    assert_eq!(store.load().await.unwrap().unwrap().client_information, None);
}

/// Pastes a redirect URL on the shared callback path instead of the server-specific one.
#[derive(Default)]
struct MixedUpPaste {
    shown: Mutex<Option<Url>>,
}

#[async_trait]
impl McpSignInPrompt for MixedUpPaste {
    fn show_authorization_url(&self, url: &Url) {
        *self.shown.lock().unwrap() = Some(url.clone());
    }

    async fn prompt_for_redirect_url(&self, _cancel: CancellationToken) -> Option<String> {
        let shown = self.shown.lock().unwrap().clone()?;
        let param = |name: &str| shown.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned());
        let mut redirect = Url::parse(&param("redirect_uri")?).ok()?;
        redirect.set_path("/callback");
        redirect.set_query(Some(&format!("code=code-1&state={}", param("state")?)));
        Some(redirect.to_string())
    }
}

#[tokio::test]
async fn accepts_the_authorization_response_only_on_the_server_specific_redirect_uri() {
    // A mixed-up authorization server redirects to the shared callback path.
    let options = OAuthServerOptions { cimd: true, redirect_path: Some("/callback".into()), ..Default::default() };
    let server = start_oauth_mcp_server(options).await;
    let error = follow(&server, cimd()).await.0.unwrap_err().to_string();
    assert!(error.contains("arrived on another redirect URI"), "{error}");

    // The same for a redirect URL pasted from the browser.
    let server = start_oauth_mcp_server(OAuthServerOptions { cimd: true, ..Default::default() }).await;
    let result = sign_in(&server, store_for(&server), cimd(), Arc::new(MixedUpPaste::default()), None).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("does not match this sign-in's redirect URI"), "{error}");
}

#[tokio::test]
async fn fails_instead_of_registering_when_the_server_does_not_support_documents() {
    let server = start_oauth_mcp_server(OAuthServerOptions { iss_parameter: true, ..Default::default() }).await;
    let error = follow(&server, cimd()).await.0.unwrap_err().to_string();
    assert!(error.contains("does not support Client ID Metadata Documents"), "{error}");
    assert!(server.state().registrations.is_empty());
}

#[tokio::test]
async fn accepts_a_pasted_redirect_url_when_the_browser_cannot_reach_the_callback() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let store = store_for(&server);
    sign_in(&server, store.clone(), McpOAuthSettings::default(), BrowserPrompt::new(Browser::Paste), None)
        .await
        .unwrap();
    assert!(server.log().contains(&"token code".to_string()));
    let token = store.load().await.unwrap().and_then(|state| state.tokens).map(|tokens| tokens.access_token);
    assert_eq!(token.as_deref(), Some("access-1"));
}

#[tokio::test]
async fn cancels_a_sign_in_waiting_on_a_stalled_authorization_server() {
    for path in ["/.well-known/oauth-authorization-server", "/token"] {
        let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
        server.state().stall.insert(path.into());
        let cancel = CancellationToken::new();
        let options = SignInOptions {
            server_url: Url::parse(&server.url).unwrap(),
            store: store_for(&server),
            settings: McpOAuthSettings::default(),
            challenge: None,
            prompt: BrowserPrompt::new(Browser::Follow),
            cancel: cancel.clone(),
            callback_factory: Arc::new(LoopbackCallbackFactory),
            client: reqwest::Client::new(),
        };
        let signing_in = tokio::spawn(sign_in_mcp_server(options));
        common::wait_until(|| server.state().stalled.len() == 1).await;
        let started = std::time::Instant::now();
        cancel.cancel();
        let result = signing_in.await.unwrap();
        assert!(matches!(result, Err(OAuthError::Cancelled)), "{path}: {result:?}");
        assert!(started.elapsed().as_millis() < 2_000);
    }
}
