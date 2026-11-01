//! Servers an ACP client supplies (`McpConfigScope::Client`): literal values, never saved, no stderr
//! or log file output, and the same tool key wins over configured and registered servers.

mod common;

use std::sync::{Arc, Mutex};

use common::fake_mcp::{Fake, FakeOptions, setup};
use common::wait_until;
use pi_mcp::manager::UpdateConfigFn;
use pi_mcp::runtime::transport::stdio_options;
use pi_mcp::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore, TransportFactory};
use pi_mcp::{
    LoadedMcpConfig, McpClient, McpClientOptions, McpConfigScope, McpExposure, McpManager, McpManagerOptions,
    McpServerEntry, McpServerRegistry, McpTransport, NotifyLevel, RequestOptions, SharedTransport, StdioTransport,
    add_client_servers,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const SERVER: &str = env!("CARGO_BIN_EXE_mcp_fake_server");

fn server(name: &str, config: Value, scope: Option<McpConfigScope>) -> McpServerEntry {
    let mut entry = McpServerEntry::new(name, serde_json::from_value(config).unwrap(), "test");
    entry.scope = scope;
    entry
}

fn client(name: &str, config: Value) -> McpServerEntry {
    let mut entry = server(name, config, Some(McpConfigScope::Client));
    entry.source = "ACP client".into();
    entry
}

/// A manager with `transports` (default: the real ones), a recorded `notify`, and its log file in `dir`.
fn real_manager(
    dir: &std::path::Path,
    servers: Vec<McpServerEntry>,
    transports: Option<TransportFactory>,
) -> (McpManager, Arc<Mutex<Vec<String>>>) {
    let notes: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = notes.clone();
    let mut options = McpManagerOptions::new(dir);
    options.credentials =
        Some(McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None));
    options.notify = Some(Arc::new(move |message: &str, _: NotifyLevel| seen.lock().unwrap().push(message.into())));
    if let Some(transports) = transports {
        options.create_transport = transports;
    }
    let manager = McpManager::new(options);
    manager.start(dir.to_path_buf(), LoadedMcpConfig { servers, ..LoadedMcpConfig::default() });
    (manager, notes)
}

#[test]
fn client_servers_replace_configured_servers_with_the_same_tool_key() {
    let global = Some(McpConfigScope::Global);
    let configured =
        vec![server("a.b", json!({ "command": "x" }), global), server("keep", json!({ "command": "y" }), global)];
    let mut loaded = LoadedMcpConfig { servers: configured.clone(), ..LoadedMcpConfig::default() };
    add_client_servers(&mut loaded, &[]);
    assert_eq!(loaded.servers, configured);
    assert!(loaded.errors.is_empty());
    add_client_servers(&mut loaded, &[client("a-b", json!({ "command": "/bin/z" }))]);
    let names: Vec<&str> = loaded.servers.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["keep", "a-b"]);
    assert_eq!(loaded.errors, ["\"a.b\" in test is replaced by the ACP client server \"a-b\" for this session"]);
}

type Connected = Arc<Mutex<Vec<String>>>;

fn with_registry(registry: &McpServerRegistry, servers: Vec<McpServerEntry>) -> (common::fake_mcp::Setup, Connected) {
    let fake = Fake::new(FakeOptions::default());
    let connected: Connected = Arc::default();
    let (seen, inner) = (connected.clone(), fake.factory());
    let factory: TransportFactory = Arc::new(move |entry, cwd, auth| {
        seen.lock().unwrap().push(entry.name.clone());
        inner(entry, cwd, auth)
    });
    let registry = registry.clone();
    let loaded = LoadedMcpConfig { servers, ..LoadedMcpConfig::default() };
    let setup = setup(fake, loaded, move |options| {
        options.create_transport = factory;
        options.registry = Some(registry);
    });
    (setup, connected)
}

#[tokio::test]
async fn client_servers_override_registered_servers_with_the_same_tool_key() {
    let registry = McpServerRegistry::new();
    registry.register("/ext/plugin.ts", "a_b", &json!({ "url": "http://plugin.invalid" })).unwrap();
    let (setup, connected) = with_registry(&registry, vec![client("a-b", json!({ "command": "/bin/z" }))]);
    setup.manager.pending().await;
    assert_eq!(*connected.lock().unwrap(), ["a-b"]);
    let notice = "\"a_b\" registered by /ext/plugin.ts is overridden by the ACP client server \"a-b\"";
    assert_eq!(setup.manager.overridden(), [notice]);
    setup.manager.shutdown().await;

    // Existing configured namespaces still override registered servers without a client.
    let configured = server("a-b", json!({ "url": "http://config.invalid" }), None);
    let (setup, connected) = with_registry(&registry, vec![configured]);
    setup.manager.pending().await;
    assert_eq!(*connected.lock().unwrap(), ["a-b"]);
    assert_eq!(setup.manager.overridden(), ["\"a_b\" registered by /ext/plugin.ts is overridden by \"a-b\" in test"]);
    setup.manager.shutdown().await;

    let (setup, connected) = with_registry(&registry, vec![]);
    setup.manager.pending().await;
    assert_eq!(*connected.lock().unwrap(), ["a_b"]);
    assert!(setup.manager.overridden().is_empty());
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn changes_to_client_servers_are_never_saved() {
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = calls.clone();
    let update: UpdateConfigFn = Arc::new(move |entry, _| {
        recorded.lock().unwrap().push(entry.name.clone());
        Ok(())
    });
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pi").join("mcp.json");
    let loaded = LoadedMcpConfig {
        servers: vec![client("docs", json!({ "command": "/bin/docs" }))],
        project_config: Some(project.clone()),
        ..LoadedMcpConfig::default()
    };
    let setup = setup(Fake::new(FakeOptions::default()), loaded, move |options| options.update_config = Some(update));
    wait_until(|| setup.manager.tool("mcp__docs__search").is_some()).await;
    assert_eq!(setup.manager.set_exposure("docs", McpExposure::Deferred), None);
    assert_eq!(setup.manager.set_enabled("docs", false, true).await, None);
    assert_eq!(setup.manager.set_enabled("docs", true, false).await, None);
    assert!(calls.lock().unwrap().is_empty());
    let status = setup.manager.status("docs").unwrap();
    assert_eq!((status.entry.override_path, status.entry.config.exposure), (None, Some(McpExposure::Deferred)));
    assert!(!project.exists() && !setup.dir.path().join("mcp.json").exists());
    assert!(!std::path::Path::new("ACP client").exists());
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn client_stdio_servers_get_their_values_literally() {
    let literal = "$HOME!echo hi ~/x ${PI_MCP_UNSET}";
    let entry = client(
        "literal",
        json!({ "command": SERVER, "args": ["~/arg", "$HOME"], "env": { "PI_MCP_LITERAL": literal } }),
    );
    let dir = tempfile::tempdir().unwrap();
    let options = stdio_options(&entry, dir.path()).unwrap();
    assert_eq!(options.args, ["~/arg", "$HOME"]);
    assert_eq!(options.max_stderr_bytes, 0);
    let transport = Arc::new(StdioTransport::new(options));
    let mcp = McpClient::new(McpClientOptions::new("client-test", "1.0.0"));
    mcp.connect(transport).await.unwrap();
    let arguments = json!({ "name": "PI_MCP_LITERAL" });
    let result = mcp.call_tool("env", Some(arguments), RequestOptions::default()).await.unwrap();
    assert_eq!(serde_json::to_value(result).unwrap()["content"][0]["text"], literal);
    mcp.close().await;
}

#[tokio::test]
async fn client_server_messages_stay_out_of_the_log_file() {
    for scope in [None, Some(McpConfigScope::Client)] {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _) = real_manager(dir.path(), vec![server("fixture", json!({ "command": SERVER }), scope)], None);
        wait_until(|| manager.tool("mcp__fixture__log").is_some()).await;
        let call = |name: &str, args: Value| {
            let tool = manager.tool(name).unwrap();
            async move { tool.call(args, CancellationToken::new(), None).await.unwrap() }
        };
        call("mcp__fixture__log", json!({ "data": "SENTINEL_LOG" })).await;
        // The server sends its log message before the answer, so it is handled by the next answer.
        call("mcp__fixture__echo", json!({ "text": "after" })).await;
        let log = std::fs::read_to_string(dir.path().join("mcp.log")).unwrap_or_default();
        assert_eq!(log.contains("SENTINEL_LOG"), scope.is_none(), "{scope:?}: {log}");
        manager.shutdown().await;
    }
}

#[tokio::test]
async fn spawn_errors_of_client_servers_name_the_server_only() {
    let missing = std::env::temp_dir().join("SENTINEL_PATH").join("server").display().to_string();
    for (scope, shows_path) in [(None, true), (Some(McpConfigScope::Client), false)] {
        let dir = tempfile::tempdir().unwrap();
        let (manager, _) = real_manager(dir.path(), vec![server("gone", json!({ "command": missing }), scope)], None);
        manager.pending().await;
        let error = manager.status("gone").unwrap().error.unwrap_or_default();
        assert_eq!(error.contains("SENTINEL_PATH"), shows_path, "{error}");
        if !shows_path {
            assert!(error.starts_with("spawn MCP server \"gone\" "), "{error}");
        }
        manager.shutdown().await;
    }
}

/// Stdio transports built like the default ones, kept for the test, each reporting its stderr chunks.
fn recording_transports(chunks: Arc<Mutex<String>>) -> (TransportFactory, Arc<Mutex<Vec<Arc<StdioTransport>>>>) {
    let transports: Arc<Mutex<Vec<Arc<StdioTransport>>>> = Arc::default();
    let kept = transports.clone();
    let factory: TransportFactory = Arc::new(move |entry, cwd, _| {
        let mut options = stdio_options(entry, cwd)?;
        let chunks = chunks.clone();
        options.on_stderr = Some(Arc::new(move |chunk: &str| chunks.lock().unwrap().push_str(chunk)));
        let transport = Arc::new(StdioTransport::new(options));
        kept.lock().unwrap().push(transport.clone());
        let shared: SharedTransport = transport;
        Ok(shared)
    });
    (factory, transports)
}

#[cfg(unix)]
#[tokio::test]
async fn stderr_of_failing_client_servers_stays_out_of_errors_and_notices() {
    // The server writes its argv and env to stderr, then reads stdin until the test closes it.
    let script = "echo \"$0 $SECRET\" >&2; while read line; do :; done";
    let config =
        json!({ "command": "/bin/sh", "args": ["-c", script, "SENTINEL_ARG"], "env": { "SECRET": "SENTINEL_ENV" } });
    for (scope, leaks) in [(None, true), (Some(McpConfigScope::Client), false)] {
        let dir = tempfile::tempdir().unwrap();
        let chunks: Arc<Mutex<String>> = Arc::default();
        let (factory, transports) = recording_transports(chunks.clone());
        let servers = vec![server("echoer", config.clone(), scope)];
        let (manager, notes) = real_manager(dir.path(), servers, Some(factory));
        let read = |count: usize| chunks.lock().unwrap().matches("SENTINEL_ARG SENTINEL_ENV").count() >= count;
        let transport = |index: usize| transports.lock().unwrap()[index].clone();
        wait_until(|| read(1)).await;
        assert_eq!(transport(0).stderr_text().contains("SENTINEL_ENV"), leaks, "{scope:?}");
        transport(0).close().await;
        manager.pending().await;
        let mut seen = vec![manager.status("echoer").unwrap().error.unwrap_or_default(), manager.format_status()];
        let close_second = async {
            wait_until(|| read(2)).await;
            transport(1).close().await;
        };
        let (reconnected, ()) = tokio::join!(manager.reconnect("echoer"), close_second);
        seen.push(reconnected.unwrap_or_default());
        seen.extend(notes.lock().unwrap().iter().cloned());
        seen.push(std::fs::read_to_string(dir.path().join("mcp.log")).unwrap_or_default());
        let text = seen.join("\n");
        assert_eq!(text.contains("SENTINEL_ARG") && text.contains("SENTINEL_ENV"), leaks, "{scope:?}: {text}");
        assert!(leaks || !text.contains("SENTINEL"), "{text}");
        manager.shutdown().await;
    }
}
