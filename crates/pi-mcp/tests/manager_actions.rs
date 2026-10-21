//! The MCP manager: waiting for servers, status, enabling and exposure saved to `mcp.json`, and
//! OAuth sign-in. Ported from coding-agent's agent-session-mcp.test.ts and
//! agent-session-mcp-oauth.test.ts.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::fake_mcp::{Fake, FakeOptions, docs, entry, setup};
use common::oauth_server::{Browser, BrowserPrompt, OAuthServerOptions, start_oauth_mcp_server};
use common::wait_until;
use pi_agent::ToolReach;
use pi_mcp::runtime::{InMemoryAuthStorageBackend, McpOAuthCredentialStore, ServerState};
use pi_mcp::{
    LoadedMcpConfig, McpConfigScope, McpExposure, McpManager, McpManagerOptions, ToolExposure, load_mcp_config,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn slow(config: Value, delay: Duration, startup_wait: Option<Duration>) -> common::fake_mcp::Setup {
    let mut config = config;
    config["url"] = json!("http://unused.invalid");
    let loaded = LoadedMcpConfig { servers: vec![entry("slow", config)], ..LoadedMcpConfig::default() };
    let options = FakeOptions { initialize_delay: Some(delay), ..FakeOptions::default() };
    setup(Fake::new(options), loaded, |options| {
        if let Some(wait) = startup_wait {
            options.startup_wait = wait;
        }
    })
}

#[tokio::test]
async fn does_not_hold_the_first_prompt_for_codemode_servers_that_are_still_connecting() {
    let setup = slow(json!({}), Duration::MAX, None);
    let started = Instant::now();
    assert!(setup.manager.wait_for_direct_servers().await);
    assert!(started.elapsed() < Duration::from_secs(1));
    // The server is listed by name before it connects.
    assert!(setup.manager.servers_section().unwrap().ends_with("- mcp__slow (codemode)"));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn holds_the_first_prompt_for_servers_with_direct_tools() {
    let setup = slow(json!({ "exposure": "direct" }), Duration::from_millis(30), None);
    assert!(setup.manager.wait_for_direct_servers().await);
    assert_eq!(setup.manager.tool("mcp__slow__search").map(|tool| tool.exposure()), Some(ToolExposure::Direct));
    // Servers with only direct tools are left out of the servers section.
    assert_eq!(setup.manager.servers_section(), None);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn holds_the_first_prompt_for_servers_with_direct_tools_only_up_to_the_startup_wait() {
    let setup = slow(json!({ "exposure": "direct" }), Duration::MAX, Some(Duration::from_millis(20)));
    assert!(!setup.manager.wait_for_direct_servers().await);
    assert!(setup.manager.tools().is_empty());
    let expected = "MCP servers are still connecting; their tools become available once connected.";
    assert_eq!(*setup.notifications.lock().unwrap(), [expected]);
    // Only the first prompt waits.
    assert!(setup.manager.wait_for_direct_servers().await);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn waits_for_the_servers_a_codemode_script_names() {
    let setup = slow(json!({}), Duration::from_millis(30), None);
    let cancel = CancellationToken::new();
    let named = ToolReach::Words(["tools", "mcp__slow__search"].map(String::from).to_vec());
    setup.manager.wait_for_tool_call("codemode", Some(&named), &cancel).await;
    assert!(setup.manager.tool("mcp__slow__search").is_some());
    setup.manager.shutdown().await;

    let never = slow(json!({}), Duration::MAX, None);
    let started = Instant::now();
    let unrelated = ToolReach::Words(["return", "1", "1"].map(String::from).to_vec());
    never.manager.wait_for_tool_call("codemode", Some(&unrelated), &cancel).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    // tool_search waits for every server; cancelling stops the wait.
    let stop = CancellationToken::new();
    let waiting = tokio::spawn({
        let (manager, stop) = (never.manager.clone(), stop.clone());
        async move { manager.wait_for_tool_call("tool_search", Some(&ToolReach::All), &stop).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiting.is_finished());
    stop.cancel();
    waiting.await.unwrap();
    never.manager.shutdown().await;
}

#[tokio::test]
async fn reports_problems_once_and_formats_the_status() {
    let loaded = LoadedMcpConfig {
        servers: vec![
            entry("docs", json!({ "url": "http://unused.invalid", "exposure": "direct" })),
            entry("off", json!({ "command": "x", "enabled": false })),
        ],
        errors: vec!["bad entry".into()],
        ..LoadedMcpConfig::default()
    };
    let setup = setup(Fake::new(FakeOptions::default()), loaded, |_| {});
    setup.manager.pending().await;
    assert_eq!(
        *setup.notifications.lock().unwrap(),
        ["MCP servers need attention:\n  config: bad entry\nRun /mcp to fix."]
    );
    let status = setup.manager.format_status();
    assert_eq!(status, "docs: connected, 3 tools (direct)\noff: disabled (codemode)\nconfig error: bad entry");
    let docs_status = setup.manager.status("docs").unwrap();
    assert_eq!(pi_mcp::manager::describe_state(&docs_status, true), "connected · 3 tools");
    setup.manager.shutdown().await;

    let empty = McpManager::new(McpManagerOptions::new(setup.dir.path()));
    let message = empty.format_status();
    assert!(message.starts_with("No MCP servers configured. Add them to "), "{message}");
    assert!(message.ends_with("mcp.json or .pi/mcp.json."), "{message}");
}

#[tokio::test]
async fn enables_disables_and_changes_exposure_in_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir_all(cwd.join(".pi")).unwrap();
    let global = json!({ "mcpServers": { "docs": { "url": "http://unused.invalid", "exposure": "direct" } } });
    std::fs::write(dir.path().join("mcp.json"), global.to_string()).unwrap();
    std::fs::write(cwd.join(".pi").join("mcp.json"), "{}").unwrap();
    let loaded = load_mcp_config(dir.path(), &cwd, true);
    assert_eq!(loaded.servers[0].scope, Some(McpConfigScope::Global));
    let setup = setup(Fake::new(FakeOptions::default()), loaded, |_| {});
    wait_until(|| setup.manager.tool("mcp__docs__search").is_some()).await;
    let read =
        |path: &std::path::Path| -> Value { serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap() };

    assert_eq!(setup.manager.set_exposure("docs", McpExposure::Deferred), None);
    assert_eq!(read(&dir.path().join("mcp.json"))["mcpServers"]["docs"]["exposure"], "deferred");
    assert_eq!(setup.manager.tool("mcp__docs__search").unwrap().exposure(), ToolExposure::Deferred);

    // Disabling in the project writes an override and hides the tools.
    assert_eq!(setup.manager.set_enabled("docs", false, true).await, None);
    assert_eq!(read(&cwd.join(".pi").join("mcp.json"))["mcpServers"]["docs"], json!({ "enabled": false }));
    assert_eq!(setup.manager.tool("mcp__docs__search").unwrap().exposure(), ToolExposure::Hidden);
    assert_eq!(setup.manager.connection("docs").map(|connection| connection.state()), None);
    let error = setup.manager.tool("mcp__docs__search").unwrap().call(json!({}), CancellationToken::new(), None).await;
    assert_eq!(error.unwrap_err().to_string(), "MCP server \"docs\" is disabled.");

    assert_eq!(setup.manager.set_enabled("docs", true, true).await, None);
    assert_eq!(read(&cwd.join(".pi").join("mcp.json"))["mcpServers"]["docs"], json!({ "enabled": true }));
    wait_until(|| setup.manager.tool("mcp__docs__search").unwrap().exposure() == ToolExposure::Deferred).await;
    assert_eq!(setup.fake.servers.lock().unwrap().len(), 2);
    assert_eq!(setup.manager.reconnect("docs").await, None);
    assert_eq!(setup.manager.reconnect("nope").await.as_deref(), Some("No MCP server named \"nope\"."));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn signs_in_through_the_browser_refreshes_and_signs_out() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(InMemoryAuthStorageBackend::default());
    let mut options = McpManagerOptions::new(dir.path());
    options.credentials = Some(McpOAuthCredentialStore::with_backend(backend.clone(), None));
    let notifications: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    let seen = notifications.clone();
    options.notify = Some(Arc::new(move |message: &str, _| seen.lock().unwrap().push(message.into())));
    let manager = McpManager::new(options);
    let config = json!({ "url": server.url, "exposure": "direct" });
    manager.start(dir.path().into(), LoadedMcpConfig { servers: vec![entry("issues", config)], ..Default::default() });
    manager.pending().await;
    let needs = "MCP servers need attention:\n  issues: needs sign-in\nRun /mcp to fix.";
    assert_eq!(*notifications.lock().unwrap(), [needs]);
    assert_eq!(manager.format_status(), "issues: needs sign-in, run /mcp login issues (direct)");

    let prompt = BrowserPrompt::new(Browser::Follow);
    assert_eq!(manager.sign_in("issues", prompt.clone(), None).await, None);
    assert_eq!(*prompt.servers.lock().unwrap(), ["issues"]);
    assert_eq!(server.log(), ["401 none", "register", "token code"]);
    let whoami = manager.tool("mcp__issues__whoami").unwrap();
    let call = || whoami.call(json!({}), CancellationToken::new(), None);
    assert_eq!(call().await.unwrap().content, [pi_ai::ContentBlock::text("token access-1")]);
    server.expire_access_tokens();
    assert_eq!(call().await.unwrap().content, [pi_ai::ContentBlock::text("token access-2")]);

    assert!(manager.sign_out("issues").await);
    let error = call().await.unwrap_err().to_string();
    assert_eq!(error, "MCP server \"issues\" requires sign-in. Run /mcp to sign in.");
    assert!(!manager.sign_out("issues").await);

    // `pi mcp login` in another process stores credentials; the next turn reconnects.
    let other = McpOAuthCredentialStore::with_backend(backend.clone(), None);
    let store = other.for_server("issues", &server.url);
    let prompt = BrowserPrompt::new(Browser::Follow);
    let options = pi_mcp::runtime::SignInOptions {
        server_name: "issues".into(),
        server_url: url::Url::parse(&server.url).unwrap(),
        store,
        settings: Default::default(),
        challenge: None,
        prompt,
        cancel: CancellationToken::new(),
        callback_factory: Arc::new(pi_mcp::oauth::LoopbackCallbackFactory),
        client: reqwest::Client::new(),
    };
    pi_mcp::runtime::sign_in_mcp_server(options).await.unwrap();
    manager.reconnect_signed_in().await;
    assert_eq!(manager.connection("issues").unwrap().state(), ServerState::Connected);
    let text = call().await.unwrap().content;
    assert!(matches!(&text[..], [pi_ai::ContentBlock::Text { .. }]));
    manager.shutdown().await;
}

#[tokio::test]
async fn cancels_a_sign_in_when_the_session_shuts_down() {
    let server = start_oauth_mcp_server(OAuthServerOptions::default()).await;
    server.state().stall.insert("/token".into());
    let config = json!({ "url": server.url });
    let setup = docs(json!({ "url": "http://unused.invalid" }), FakeOptions::default());
    setup.manager.shutdown().await;
    let mut options = McpManagerOptions::new(setup.dir.path());
    options.credentials =
        Some(McpOAuthCredentialStore::with_backend(Arc::new(InMemoryAuthStorageBackend::default()), None));
    let manager = McpManager::new(options);
    manager.start(
        setup.dir.path().into(),
        LoadedMcpConfig { servers: vec![entry("issues", config)], ..Default::default() },
    );
    manager.pending().await;
    let signing_in = tokio::spawn({
        let manager = manager.clone();
        async move { manager.sign_in("issues", BrowserPrompt::new(Browser::Follow), None).await }
    });
    wait_until(|| server.state().stalled.len() == 1).await;
    let started = Instant::now();
    manager.shutdown().await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(signing_in.await.unwrap().as_deref(), Some("Sign-in cancelled."));
}
