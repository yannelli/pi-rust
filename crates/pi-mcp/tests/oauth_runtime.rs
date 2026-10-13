//! OAuth at runtime: refreshes shared across processes, the credential store, and connections that
//! sign in, refresh, and sign out. Ported from coding-agent's mcp-oauth-refresh.test.ts,
//! mcp-oauth-store.test.ts, and agent-session-mcp-oauth.test.ts.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::oauth_server::{Browser, BrowserPrompt, OAuthMcpServer, OAuthServerOptions, start_oauth_mcp_server};
use pi_mcp::oauth::{LoopbackCallbackFactory, McpOAuthState, McpOAuthStateStore, OAuthTokens};
use pi_mcp::runtime::{
    AuthStorageBackend, ConnectionOptions, InMemoryAuthStorageBackend, McpAuthProvider, McpOAuthCredentialStore,
    McpOAuthServerStore, McpOAuthSettings, McpServerConnection, ServerState, SignInOptions, sign_in_mcp_server,
};
use pi_mcp::{AuthProvider, McpServerEntry, RequestOptions, UnauthorizedContext};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

async fn sign_in(server: &OAuthMcpServer, store: Arc<McpOAuthServerStore>) {
    sign_in_mcp_server(SignInOptions {
        server_name: "test".into(),
        server_url: Url::parse(&server.url).unwrap(),
        store,
        settings: McpOAuthSettings::default(),
        challenge: None,
        prompt: BrowserPrompt::new(Browser::Follow),
        cancel: CancellationToken::new(),
        callback_factory: Arc::new(LoopbackCallbackFactory),
        client: reqwest::Client::new(),
    })
    .await
    .unwrap();
}

/// A pi process: its own store and auth provider over the shared credential file and lock directory.
fn process(
    server: &OAuthMcpServer,
    backend: &Arc<InMemoryAuthStorageBackend>,
    lock_dir: &Path,
) -> (Arc<McpOAuthServerStore>, Arc<McpAuthProvider>) {
    let credentials = McpOAuthCredentialStore::with_backend(backend.clone(), Some(lock_dir.into()));
    let store = credentials.for_server("test", &server.url);
    let provider = McpAuthProvider::new(
        Url::parse(&server.url).unwrap(),
        store.clone(),
        Arc::new(|| Ok(McpOAuthSettings::default())),
        Arc::new(|_| {}),
        reqwest::Client::new(),
    );
    (store, Arc::new(provider))
}

fn rejected(server: &OAuthMcpServer, token: &str) -> UnauthorizedContext {
    UnauthorizedContext {
        status: 401,
        headers: HeaderMap::new(),
        server_url: Url::parse(&server.url).unwrap(),
        client: reqwest::Client::new(),
        token: Some(token.into()),
    }
}

#[tokio::test]
async fn refreshes_once_when_several_processes_find_the_same_token_rejected() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let lock_dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    sign_in(&server, process(&server, &backend, lock_dir.path()).0).await;
    server.expire_access_tokens();
    let processes: Vec<_> = (0..3).map(|_| process(&server, &backend, lock_dir.path())).collect();
    // The server rotates refresh tokens: a second refresh with refresh-1 would fail with invalid_grant.
    let refreshes = processes.iter().map(|(_, provider)| provider.on_unauthorized(rejected(&server, "access-1")));
    for result in futures::future::join_all(refreshes).await {
        assert!(result.unwrap());
    }
    assert_eq!(server.log().iter().filter(|entry| *entry == "token refresh").count(), 1);
    for (_, provider) in &processes {
        assert_eq!(provider.token().await.unwrap().as_deref(), Some("access-2"));
    }
    // The lock is released.
    assert_eq!(std::fs::read_dir(lock_dir.path()).unwrap().count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn waits_for_a_running_refresh_to_save_the_new_tokens() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let lock_dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    sign_in(&server, process(&server, &backend, lock_dir.path()).0).await;
    server.expire_access_tokens();
    let (store, provider) = process(&server, &backend, lock_dir.path());
    let refreshing = provider.clone();
    let context = rejected(&server, "access-1");
    let refresh = tokio::spawn(async move { refreshing.on_unauthorized(context).await });
    // Let the spawned call start its refresh.
    tokio::task::yield_now().await;
    provider.settled().await;
    let token = store.load().await.unwrap().and_then(|state| state.tokens).map(|tokens| tokens.access_token);
    assert_eq!(token.as_deref(), Some("access-2"));
    assert!(refresh.await.unwrap().unwrap());
}

const SERVER_URL: &str = "https://mcp.example.com/mcp";

fn state(access_token: &str) -> McpOAuthState {
    let tokens =
        OAuthTokens { access_token: access_token.into(), token_type: "Bearer".into(), ..OAuthTokens::default() };
    McpOAuthState { server_url: SERVER_URL.into(), tokens: Some(tokens), ..McpOAuthState::default() }
}

fn stored_keys(backend: &InMemoryAuthStorageBackend) -> Vec<String> {
    let content: Value = serde_json::from_str(&backend.content().unwrap_or_else(|| "{}".into())).unwrap();
    content.as_object().unwrap().keys().cloned().collect()
}

fn legacy_backend() -> Arc<InMemoryAuthStorageBackend> {
    let content = json!({ SERVER_URL: serde_json::to_value(state("legacy-token")).unwrap() });
    Arc::new(InMemoryAuthStorageBackend::with_content(content.to_string()))
}

async fn access_token(store: &McpOAuthServerStore) -> Option<String> {
    store.load().await.unwrap().and_then(|state| state.tokens).map(|tokens| tokens.access_token)
}

#[tokio::test]
async fn keeps_separate_credentials_for_servers_sharing_a_url() {
    let store = McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None);
    store.for_server("work", SERVER_URL).save(state("work-token")).await.unwrap();
    store.for_server("personal", SERVER_URL).save(state("personal-token")).await.unwrap();
    assert_eq!(access_token(&store.for_server("work", SERVER_URL)).await.as_deref(), Some("work-token"));
    assert_eq!(access_token(&store.for_server("personal", SERVER_URL)).await.as_deref(), Some("personal-token"));

    assert!(store.remove("work", SERVER_URL));
    assert_eq!(store.for_server("work", SERVER_URL).load().await.unwrap(), None);
    assert_eq!(store.tokens("personal", SERVER_URL).unwrap().access_token, "personal-token");
}

#[tokio::test]
async fn moves_credentials_stored_by_url_to_the_first_server_that_loads_them() {
    let backend = legacy_backend();
    let store = McpOAuthCredentialStore::with_backend(backend.clone(), None);
    // Reading tokens does not take the legacy state over.
    assert_eq!(store.tokens("work", SERVER_URL).unwrap().access_token, "legacy-token");
    assert_eq!(stored_keys(&backend), [SERVER_URL]);

    assert_eq!(access_token(&store.for_server("my_work", SERVER_URL)).await.as_deref(), Some("legacy-token"));
    // Names differing only in `-` and `_` are the same server.
    assert_eq!(store.tokens("my-work", SERVER_URL).unwrap().access_token, "legacy-token");
    assert_eq!(store.for_server("personal", SERVER_URL).load().await.unwrap(), None);
    assert_eq!(stored_keys(&backend), [format!("mcp__my_work|{SERVER_URL}")]);
}

#[test]
fn signs_out_of_credentials_stored_by_url() {
    let backend = legacy_backend();
    let store = McpOAuthCredentialStore::with_backend(backend.clone(), None);
    assert!(store.remove("work", SERVER_URL));
    assert!(stored_keys(&backend).is_empty());
    assert!(!store.remove("work", SERVER_URL));
}

struct Session {
    server: OAuthMcpServer,
    backend: Arc<InMemoryAuthStorageBackend>,
    credentials: McpOAuthCredentialStore,
    connection: McpServerConnection,
}

async fn session() -> Session {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    let credentials = McpOAuthCredentialStore::with_backend(backend.clone(), None);
    let config = serde_json::from_value(json!({ "url": server.url, "exposure": "direct" })).unwrap();
    let entry = McpServerEntry::new("issues", config, "test");
    let connection = McpServerConnection::new(ConnectionOptions::new(entry, std::env::temp_dir(), credentials.clone()));
    Session { server, backend, credentials, connection }
}

async fn whoami(connection: &McpServerConnection) -> Result<String, String> {
    let result = connection.call_tool("whoami", json!({}), RequestOptions::default()).await;
    let result = serde_json::to_value(result.map_err(|error| error.to_string())?).unwrap();
    Ok(result["content"][0]["text"].as_str().unwrap_or_default().to_string())
}

fn set_tokens_expire_at(backend: &InMemoryAuthStorageBackend, expires_at: i64) {
    backend
        .with_lock(&mut |current| {
            let mut states: Value = serde_json::from_str(current.unwrap_or("{}")).unwrap();
            for state in states.as_object_mut().unwrap().values_mut() {
                state["tokensExpireAt"] = json!(expires_at);
            }
            Some(states.to_string())
        })
        .unwrap();
}

#[tokio::test]
async fn signs_in_through_the_browser_refreshes_expired_tokens_and_signs_out() {
    let Session { server, backend, credentials, connection } = session().await;
    let error = connection.get_client().await.unwrap_err().to_string();
    assert_eq!(error, "MCP server \"issues\" requires sign-in. Run /mcp to sign in.");
    assert_eq!(connection.state(), ServerState::NeedsAuth);

    sign_in(&server, credentials.for_server("issues", &server.url)).await;
    connection.reconnect().await.unwrap();
    assert_eq!(connection.tools().len(), 1);
    assert_eq!(server.log(), ["401 none", "register", "token code"]);
    assert!(backend.content().unwrap().contains("\"access_token\": \"access-1\""));
    assert_eq!(whoami(&connection).await.unwrap(), "token access-1");

    // An expired access token is refreshed without user interaction.
    server.expire_access_tokens();
    assert_eq!(whoami(&connection).await.unwrap(), "token access-2");
    assert_eq!(server.log()[server.log().len() - 3..], ["401 access-1", "token refresh", "call access-2"]);

    // A token past its expiry is refreshed before the request, without a 401 round trip.
    set_tokens_expire_at(&backend, pi_ai::now_ms() - 1_000);
    assert_eq!(whoami(&connection).await.unwrap(), "token access-3");
    assert_eq!(server.log()[server.log().len() - 2..], ["token refresh", "call access-3"]);

    assert!(credentials.remove("issues", &server.url));
    connection.sign_out().await;
    assert_eq!(connection.state(), ServerState::NeedsAuth);
    let error = whoami(&connection).await.unwrap_err();
    assert_eq!(error, "MCP server \"issues\" requires sign-in. Run /mcp to sign in.");
    connection.close().await;
}

#[tokio::test]
async fn closes_without_refreshing_an_expiring_token() {
    let Session { server, backend, credentials, connection } = session().await;
    sign_in(&server, credentials.for_server("issues", &server.url)).await;
    connection.get_client().await.unwrap();
    // Still accepted, but close enough to expiry that the next request would refresh it first.
    set_tokens_expire_at(&backend, pi_ai::now_ms() + 10_000);
    server.state().stall.insert("/token".into());
    let started = Instant::now();
    connection.close().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(server.state().stalled.is_empty());
    assert_eq!(server.state().deletes, [Some("access-1".to_string())]);
}
