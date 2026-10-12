//! Server connections: reconnects, retries, sign-in states, and logs. Ported from the
//! "MCP connections" cases of coding-agent's mcp-extension.test.ts.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::http::{Response, serve};
use common::{TestServer, create_server, settle, wait_until};
use futures::FutureExt;
use pi_mcp::runtime::{
    ConnectionOptions, InMemoryAuthStorageBackend, McpOAuthCredentialStore, McpServerConnection, ServerState,
    TransportFactory,
};
use pi_mcp::{
    InMemoryTransport, JsonRpcMessage, McpError, McpServerConfig, McpServerEntry, McpServerLog, McpTransport,
    RequestOptions, SharedTransport, TransportSink,
};
use serde_json::{Value, json};

type SendHook = Arc<dyn Fn(&JsonRpcMessage) -> Option<McpError> + Send + Sync>;

/// An in-memory transport whose sends can fail, like an HTTP transport would.
struct Hooked {
    inner: Arc<InMemoryTransport>,
    hook: SendHook,
}

#[async_trait]
impl McpTransport for Hooked {
    async fn start(&self, sink: TransportSink) -> Result<(), McpError> {
        self.inner.start(sink).await
    }

    async fn send(&self, message: &JsonRpcMessage) -> Result<(), McpError> {
        match (self.hook)(message) {
            Some(error) => Err(error),
            None => self.inner.send(message).await,
        }
    }

    async fn close(&self) {
        self.inner.close().await;
    }
}

fn http_error(status: u16, message: &str) -> McpError {
    McpError::Http { status, message: message.into(), body: String::new() }
}

/// A server answering tools/list with no tools, tools/call with "ok", and resources/read.
async fn fake_server() -> (Arc<InMemoryTransport>, TestServer) {
    let (transport, server) = create_server().await;
    server.set_result("tools/list", json!({ "tools": [] }));
    server.set_result("tools/call", json!({ "content": [{ "type": "text", "text": "ok" }] }));
    server.set_result("resources/read", json!({ "contents": [{ "uri": "docs://a", "text": "ok" }] }));
    (transport, server)
}

fn entry(config: Value) -> McpServerEntry {
    McpServerEntry::new("fake", serde_json::from_value::<McpServerConfig>(config).unwrap(), "test")
}

fn credentials() -> McpOAuthCredentialStore {
    McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None)
}

type Opener = Box<dyn Fn() -> SharedTransport + Send + Sync>;

/// A connection whose n-th connect uses the n-th transport.
fn connect_with(entry: McpServerEntry, transports: Vec<Opener>) -> (McpServerConnection, Arc<AtomicUsize>) {
    let opened = Arc::new(AtomicUsize::new(0));
    let count = opened.clone();
    let factory: TransportFactory = Arc::new(move |_, _, _| Ok(transports[count.fetch_add(1, Ordering::SeqCst)]()));
    let mut options = ConnectionOptions::new(entry, std::env::current_dir().unwrap(), credentials());
    options.create_transport = factory;
    (McpServerConnection::new(options), opened)
}

fn stdio() -> McpServerEntry {
    entry(json!({ "command": "unused" }))
}

fn opener(transport: Arc<InMemoryTransport>) -> Opener {
    let transport = Mutex::new(Some(transport));
    Box::new(move || transport.lock().unwrap().take().unwrap() as SharedTransport)
}

fn hooked(
    transport: Arc<InMemoryTransport>,
    hook: impl Fn(&JsonRpcMessage) -> Option<McpError> + Send + Sync + 'static,
) -> Opener {
    let transport = Mutex::new(Some(transport));
    let hook: SendHook = Arc::new(hook);
    Box::new(move || Arc::new(Hooked { inner: transport.lock().unwrap().take().unwrap(), hook: hook.clone() }))
}

fn ok_text() -> Value {
    json!({ "content": [{ "type": "text", "text": "ok" }] })
}

async fn call(connection: &McpServerConnection) -> Result<Value, McpError> {
    let result = connection.call_tool("echo", json!({}), RequestOptions::default()).await?;
    Ok(serde_json::to_value(result).unwrap())
}

#[tokio::test]
async fn starts_a_new_session_and_retries_once_when_the_session_expired() {
    let (first, _a) = fake_server().await;
    let (second, _b) = fake_server().await;
    let expire = |message: &JsonRpcMessage| {
        (message.method() == Some("tools/call")).then(|| McpError::SessionExpired { body: "gone".into() })
    };
    let (connection, opened) = connect_with(stdio(), vec![hooked(first, expire), opener(second)]);
    let (a, b) = futures::join!(call(&connection), call(&connection));
    assert_eq!(a.unwrap(), ok_text());
    assert_eq!(b.unwrap(), ok_text());
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    connection.close().await;
}

#[tokio::test]
async fn connects_to_servers_without_the_tools_capability_without_listing_tools() {
    let (transport, server) = fake_server().await;
    server.set_result(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": { "prompts": {} }, "serverInfo": { "name": "fake", "version": "1" } }),
    );
    let (connection, _) = connect_with(stdio(), vec![opener(transport)]);
    connection.get_client().await.unwrap();
    assert_eq!(connection.state(), ServerState::Connected);
    assert!(connection.tools().is_empty());
    let requests: Vec<_> = server
        .messages()
        .iter()
        .filter(|message| message.id().is_some())
        .filter_map(|message| message.method().map(str::to_string))
        .collect();
    assert_eq!(requests, ["initialize"]);
    connection.close().await;
}

#[tokio::test]
async fn marks_a_dropped_connection_and_reconnects_on_the_next_call() {
    let (first, first_server) = fake_server().await;
    let (second, _b) = fake_server().await;
    let (connection, opened) = connect_with(stdio(), vec![opener(first), opener(second)]);
    connection.get_client().await.unwrap();
    first_server.transport.close().await;
    wait_until(|| connection.state() == ServerState::Disconnected).await;
    assert_eq!(connection.error().as_deref(), Some("Connection closed"));
    assert_eq!(call(&connection).await.unwrap(), ok_text());
    assert_eq!(connection.state(), ServerState::Connected);
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    connection.close().await;
}

fn http_entry() -> McpServerEntry {
    entry(json!({ "url": "http://unused.invalid", "headers": { "Authorization": "x" } }))
}

#[tokio::test]
async fn retries_http_connections_that_fail_with_a_transient_error() {
    let (first, _a) = fake_server().await;
    let (second, _b) = fake_server().await;
    let unavailable = |_: &JsonRpcMessage| Some(http_error(503, "MCP HTTP request failed with status 503"));
    let (connection, opened) = connect_with(http_entry(), vec![hooked(first, unavailable), opener(second)]);
    connection.get_client().await.unwrap();
    assert_eq!(connection.state(), ServerState::Connected);
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    connection.close().await;

    let (third, _c) = fake_server().await;
    let bad = |_: &JsonRpcMessage| Some(http_error(400, "MCP HTTP request failed with status 400: bad"));
    let (failing, opened) = connect_with(http_entry(), vec![hooked(third, bad)]);
    let error = failing.get_client().await.unwrap_err().to_string();
    assert!(error.contains("status 400: bad"), "{error}");
    assert_eq!(failing.state(), ServerState::Failed);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn closes_a_connection_that_is_still_initializing_before_close_returns() {
    let (transport, server) = create_server().await;
    server.set_handler("initialize", |_| futures::future::pending::<Result<Value, McpError>>());
    let client_transport = transport.clone();
    let (connection, _) = connect_with(stdio(), vec![opener(transport)]);
    let opening = tokio::spawn({
        let connection = connection.clone();
        async move { connection.get_client().await.map(|_| ()) }
    });
    wait_until(|| server.methods().contains(&"initialize".to_string())).await;
    connection.close().await;
    assert!(client_transport.is_closed());
    let error = opening.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("failed to connect"), "{error}");
    assert_eq!(connection.state(), ServerState::Closed);
}

#[tokio::test]
async fn stops_waiting_to_retry_a_connection_when_closed() {
    let (transport, _server) = fake_server().await;
    let unavailable = |_: &JsonRpcMessage| Some(http_error(503, "MCP HTTP request failed with status 503"));
    let (connection, _) = connect_with(http_entry(), vec![hooked(transport, unavailable)]);
    let opening = tokio::spawn({
        let connection = connection.clone();
        async move { connection.get_client().await.map(|_| ()) }
    });
    // The first try failed; the retry is 250 ms away.
    tokio::time::sleep(Duration::from_millis(30)).await;
    let started = std::time::Instant::now();
    connection.close().await;
    assert!(started.elapsed() < Duration::from_millis(200));
    let error = opening.await.unwrap().unwrap_err().to_string();
    assert!(error.contains("status 503"), "{error}");
}

#[tokio::test]
async fn retries_resource_reads_but_not_tool_calls_after_a_transient_http_error() {
    let (transport, _server) = fake_server().await;
    let failed = Mutex::new(Vec::<String>::new());
    let flaky = move |message: &JsonRpcMessage| {
        let method = message.method().unwrap_or_default().to_string();
        let mut failed = failed.lock().unwrap();
        if (method == "resources/read" || method == "tools/call") && !failed.contains(&method) {
            failed.push(method);
            return Some(http_error(502, "MCP HTTP request failed with status 502"));
        }
        None
    };
    let (connection, _) = connect_with(stdio(), vec![hooked(transport, flaky)]);
    let read = connection.read_resource("docs://a", RequestOptions::default()).await.unwrap();
    assert_eq!(serde_json::to_value(read).unwrap(), json!({ "contents": [{ "uri": "docs://a", "text": "ok" }] }));
    let error = call(&connection).await.unwrap_err().to_string();
    assert!(error.contains("status 502"), "{error}");
    connection.close().await;
}

#[tokio::test]
async fn asks_oauth_servers_that_keep_rejecting_requests_for_a_new_sign_in() {
    let (transport, _server) = fake_server().await;
    let rejected = |_: &JsonRpcMessage| Some(McpError::AuthRequired { body: String::new(), www_authenticate: None });
    let (connection, _) =
        connect_with(entry(json!({ "url": "http://unused.invalid" })), vec![hooked(transport, rejected)]);
    let error = connection.get_client().await.unwrap_err().to_string();
    assert_eq!(error, "MCP server \"fake\" requires sign-in. Run /mcp to sign in.");
    assert_eq!(connection.state(), ServerState::NeedsAuth);
    connection.close().await;
}

#[tokio::test]
async fn sends_the_provider_token_and_asks_for_the_provider_login_when_the_server_rejects_it() {
    let authorizations = Arc::new(Mutex::new(Vec::new()));
    let seen = authorizations.clone();
    let origin = serve(move |request, _| {
        seen.lock().unwrap().push(request.header("authorization").map(str::to_string));
        async { Response::status(401) }
    })
    .await;
    let config = json!({ "url": format!("{origin}/mcp"), "auth": { "provider": "radius" } });
    let entry = McpServerEntry::new("radius", serde_json::from_value(config).unwrap(), "test");
    let mut options = ConnectionOptions::new(entry, std::env::current_dir().unwrap(), credentials());
    options.provider_token =
        Some(Arc::new(|provider: String| async move { (provider == "radius").then(|| "tok".to_string()) }.boxed()));
    let connection = McpServerConnection::new(options);
    assert_eq!(connection.oauth_url(), None);
    let error = connection.get_client().await.unwrap_err().to_string();
    assert_eq!(error, "MCP server \"radius\" requires sign-in. Run /login radius to sign in.");
    assert_eq!(connection.state(), ServerState::NeedsAuth);
    assert_eq!(*authorizations.lock().unwrap(), [Some("Bearer tok".to_string())]);
    connection.close().await;
}

#[tokio::test]
async fn appends_server_log_messages_to_the_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.log");
    let (transport, server) = fake_server().await;
    let opened = Arc::new(AtomicUsize::new(0));
    let factory_transport = Mutex::new(Some(transport));
    let mut options = ConnectionOptions::new(stdio(), dir.path().into(), credentials());
    let count = opened.clone();
    options.create_transport = Arc::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(factory_transport.lock().unwrap().take().unwrap() as SharedTransport)
    });
    options.log = Some(Arc::new(McpServerLog::new(&path)));
    let connection = McpServerConnection::new(options);
    connection.get_client().await.unwrap();
    let params = json!({ "level": "warning", "logger": "db", "data": "slow\nquery" });
    server.send(JsonRpcMessage::notification("notifications/message", Some(params))).await;
    let params = json!({ "level": "error", "data": { "code": 7 } });
    server.send(JsonRpcMessage::notification("notifications/message", Some(params))).await;
    wait_until(|| std::fs::read_to_string(&path).is_ok_and(|text| text.lines().count() == 3)).await;
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = text
        .lines()
        .map(|line| line.split_once(' ').map_or(line, |(first, rest)| if first.ends_with('Z') { rest } else { line }))
        .collect();
    assert_eq!(lines, ["[fake] warning db: slow", "    query", "[fake] error {\"code\":7}"]);
    connection.close().await;
}

#[tokio::test]
async fn resolves_the_oauth_client_secret_lazily() {
    let (transport, _server) = fake_server().await;
    let config = json!({ "url": "http://unused.invalid", "oauth": { "clientSecret": "!exit 1" } });
    let (connection, _) = connect_with(entry(config), vec![opener(transport)]);
    assert_eq!(call(&connection).await.unwrap(), ok_text());
    let error = connection.oauth_settings().unwrap_err();
    assert!(error.contains("oauth.clientSecret"), "{error}");
    connection.close().await;
}

#[tokio::test]
async fn refreshes_tools_and_resources_when_the_server_says_they_changed() {
    let (transport, server) = fake_server().await;
    server.set_result(
        "initialize",
        json!({ "protocolVersion": "2025-06-18", "capabilities": { "tools": {}, "resources": {} }, "serverInfo": { "name": "fake", "version": "1" }, "instructions": "  Docs server.  " }),
    );
    server.set_result(
        "resources/list",
        json!({ "resources": [{ "uri": "docs://a", "name": "a" }, { "uri": "ui://app", "name": "app" }, { "uri": "docs://b", "name": "b", "mimeType": "text/html;profile=mcp-app" }] }),
    );
    let tool_updates = Arc::new(AtomicUsize::new(0));
    let updates = tool_updates.clone();
    let mut options = ConnectionOptions::new(stdio(), std::env::current_dir().unwrap(), credentials());
    let factory_transport = Mutex::new(Some(transport));
    options.create_transport =
        Arc::new(move |_, _, _| Ok(factory_transport.lock().unwrap().take().unwrap() as SharedTransport));
    options.on_tools = Some(Arc::new(move |_| {
        updates.fetch_add(1, Ordering::SeqCst);
    }));
    let connection = McpServerConnection::new(options);
    connection.get_client().await.unwrap();
    assert!(connection.has_resources());
    assert_eq!(connection.instructions().as_deref(), Some("Docs server."));
    let uris: Vec<_> = connection.resources().into_iter().map(|resource| resource.uri).collect();
    assert_eq!(uris, ["docs://a"]);
    assert!(connection.resource_templates().is_empty());
    assert_eq!(tool_updates.load(Ordering::SeqCst), 1);

    server.set_result("tools/list", json!({ "tools": [{ "name": "echo", "inputSchema": { "type": "object" } }] }));
    server.send(JsonRpcMessage::notification("notifications/tools/list_changed", None)).await;
    wait_until(|| connection.tools().len() == 1).await;
    server.set_result("resources/list", json!({ "resources": [] }));
    server.send(JsonRpcMessage::notification("notifications/resources/list_changed", None)).await;
    wait_until(|| connection.resources().is_empty()).await;
    settle().await;
    assert_eq!(tool_updates.load(Ordering::SeqCst), 3);
    connection.close().await;
}

#[tokio::test]
async fn runs_stdio_servers_in_the_configured_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("work")).unwrap();
    let config = json!({ "command": env!("CARGO_BIN_EXE_mcp_fake_server"), "cwd": "work" });
    let options = ConnectionOptions::new(entry(config), dir.path().into(), credentials());
    let connection = McpServerConnection::new(options);
    let result = connection.call_tool("cwd", json!({}), RequestOptions::default()).await.unwrap();
    let text = serde_json::to_value(&result).unwrap()["content"][0]["text"].as_str().unwrap().to_string();
    assert_eq!(std::fs::canonicalize(text).unwrap(), std::fs::canonicalize(dir.path().join("work")).unwrap());
    assert_eq!(connection.state(), ServerState::Connected);
    connection.close().await;
    assert_eq!(connection.state(), ServerState::Closed);
    let error = connection.get_client().await.unwrap_err().to_string();
    assert_eq!(error, "MCP server \"fake\" is shut down");
}
