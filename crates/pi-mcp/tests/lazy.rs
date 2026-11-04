//! `lazy` servers: no connection at startup, tools from the tool cache, and connecting on first use.

use crate::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::fake_mcp::{Fake, FakeOptions, Setup, entry, server_tools, setup, setup_in};
use common::wait_until;
use pi_agent::ToolReach;
use pi_mcp::config::McpServerConfigPatch;
use pi_mcp::manager::describe_state;
use pi_mcp::runtime::ServerState;
use pi_mcp::tool_cache::{connection_fingerprint, read_cached_tools};
use pi_mcp::{LoadedMcpConfig, McpConfigScope, McpError, McpLifecycle, McpServerEntry, ToolExposure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn loaded(servers: Vec<McpServerEntry>) -> LoadedMcpConfig {
    LoadedMcpConfig { servers, ..LoadedMcpConfig::default() }
}

fn docs_entry(config: Value) -> McpServerEntry {
    let mut config = config;
    config["url"] = json!("http://unused.invalid");
    entry("docs", config)
}

/// An agent directory whose tool cache holds the tools of an eager `docs` server with `config`.
async fn cached_dir(config: Value) -> tempfile::TempDir {
    let first = setup(Fake::new(FakeOptions::default()), loaded(vec![docs_entry(config)]), |_| {});
    first.manager.pending().await;
    first.manager.shutdown().await;
    first.dir
}

fn lazy(dir: tempfile::TempDir, fake: Fake, config: Value) -> Setup {
    let mut config = config;
    config["lifecycle"] = json!("lazy");
    setup_in(dir, fake, loaded(vec![docs_entry(config)]), |_| {})
}

fn connections(setup: &Setup) -> usize {
    setup.fake.servers.lock().unwrap().len()
}

fn text_of(result: &pi_agent::AgentToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap();
    let texts = content.as_array().unwrap().iter().filter_map(|block| block["text"].as_str().map(str::to_string));
    texts.collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn lazy_servers_do_not_connect_at_startup() {
    let setup = lazy(tempfile::tempdir().unwrap(), Fake::new(FakeOptions::default()), json!({}));
    setup.manager.pending().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(connections(&setup), 0);
    assert!(setup.manager.tools().is_empty());
    let status = setup.manager.status("docs").unwrap();
    assert_eq!((status.lifecycle, status.state), (McpLifecycle::Lazy, None));
    assert_eq!(describe_state(&status, true), "idle, connects on first use");
    assert_eq!(setup.manager.format_status(), "docs: idle, connects on first use (codemode)");
    assert!(setup.notifications.lock().unwrap().is_empty());
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn the_top_level_lifecycle_is_the_default() {
    let mut config = loaded(vec![docs_entry(json!({})), docs_entry(json!({ "lifecycle": "eager" }))]);
    config.servers[1].name = "eager".into();
    config.lifecycle = Some(McpLifecycle::Lazy);
    let setup = setup(Fake::new(FakeOptions::default()), config, |_| {});
    setup.manager.pending().await;
    assert_eq!(connections(&setup), 1);
    assert_eq!(setup.manager.status("docs").unwrap().lifecycle, McpLifecycle::Lazy);
    assert_eq!(setup.manager.status("eager").unwrap().state, Some(ServerState::Connected));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn saves_tool_lists_and_registers_cached_tools_at_startup() {
    let options = FakeOptions { instructions: Some("Search first.".into()), ..FakeOptions::default() };
    let first = setup(Fake::new(options), loaded(vec![docs_entry(json!({}))]), |_| {});
    first.manager.pending().await;
    first.manager.shutdown().await;
    let cached = read_cached_tools(first.dir.path(), &docs_entry(json!({}))).unwrap();
    let names: Vec<&str> = cached.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["search", "fail", "shot"]);
    assert_eq!(cached.instructions.as_deref(), Some("Search first."));

    let setup = lazy(first.dir, Fake::new(FakeOptions::default()), json!({ "toolExposure": { "shot": "direct" } }));
    let exposures: Vec<(String, ToolExposure)> =
        setup.manager.tools().iter().map(|tool| (tool.name().to_string(), tool.exposure())).collect();
    let expected = [
        ("mcp__docs__search".to_string(), ToolExposure::Deferred),
        ("mcp__docs__fail".to_string(), ToolExposure::Deferred),
        ("mcp__docs__shot".to_string(), ToolExposure::Direct),
    ];
    assert_eq!(exposures, expected);
    let search = setup.manager.tool("mcp__docs__search").unwrap();
    assert_eq!(search.namespace().unwrap().instructions.as_deref(), Some("Search first."));
    assert!(setup.manager.servers_section().unwrap().ends_with("- mcp__docs (codemode): Search first."));
    // tool_search finds cached tools without connecting.
    let cancel = CancellationToken::new();
    setup.manager.wait_for_tool_call("tool_search", Some(&ToolReach::All), &cancel).await;
    // The first prompt does not connect a cached server with direct tools.
    assert!(setup.manager.wait_for_direct_servers().await);
    assert_eq!(connections(&setup), 0);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn a_tool_call_connects_the_server_and_uses_its_live_tools() {
    let dir = cached_dir(json!({})).await;
    let fake = Fake::new(FakeOptions::default());
    let without_shot: Vec<Value> =
        server_tools().as_array().unwrap().iter().filter(|tool| tool["name"] != "shot").cloned().collect();
    *fake.tools.lock().unwrap() = Value::Array(without_shot);
    let setup = lazy(dir, fake, json!({}));
    let search = setup.manager.tool("mcp__docs__search").unwrap();
    let args = json!({ "query": "lazy" });
    let result = search.agent_tool().execute("call", args, CancellationToken::new(), Arc::new(|_| {})).await.unwrap();
    assert_eq!(text_of(&result), "lazy guide\nlazy faq");
    assert_eq!(connections(&setup), 1);
    assert_eq!(setup.fake.calls(), [r#"search:{"query":"lazy"}"#]);
    assert_eq!(setup.manager.status("docs").unwrap().state, Some(ServerState::Connected));
    // The live list replaces the cached one, in the registry and in the cache.
    assert_eq!(setup.manager.tool("mcp__docs__shot").unwrap().exposure(), ToolExposure::Hidden);
    let cached = read_cached_tools(setup.dir.path(), &docs_entry(json!({ "lifecycle": "lazy" }))).unwrap();
    assert_eq!(cached.tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(), ["search", "fail"]);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn a_failed_connection_fails_the_call_with_the_server_and_reason() {
    let dir = cached_dir(json!({})).await;
    let entries = loaded(vec![docs_entry(json!({ "lifecycle": "lazy" }))]);
    let setup = setup_in(dir, Fake::new(FakeOptions::default()), entries, |options| {
        options.create_transport = Arc::new(|_, _, _| Err(McpError::other("spawn failed")));
    });
    let search = setup.manager.tool("mcp__docs__search").unwrap();
    let args = json!({ "query": "x" });
    let error = search.agent_tool().execute("call", args, CancellationToken::new(), Arc::new(|_| {})).await;
    assert_eq!(error.unwrap_err().to_string(), "MCP server \"docs\" failed to connect: spawn failed");
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn tool_search_connects_lazy_servers_without_a_cache() {
    let setup = lazy(tempfile::tempdir().unwrap(), Fake::new(FakeOptions::default()), json!({}));
    let cancel = CancellationToken::new();
    // A script that names no server does not connect it.
    let unrelated = ToolReach::Words(vec!["return".into()]);
    setup.manager.wait_for_tool_call("codemode", Some(&unrelated), &cancel).await;
    assert_eq!(connections(&setup), 0);
    setup.manager.wait_for_tool_call("tool_search", Some(&ToolReach::All), &cancel).await;
    assert_eq!(connections(&setup), 1);
    assert_eq!(setup.manager.tool("mcp__docs__search").map(|tool| tool.exposure()), Some(ToolExposure::Deferred));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn the_first_prompt_connects_lazy_direct_servers_without_a_cache() {
    let setup = lazy(tempfile::tempdir().unwrap(), Fake::new(FakeOptions::default()), json!({ "exposure": "direct" }));
    assert!(setup.manager.wait_for_direct_servers().await);
    assert_eq!(setup.manager.tool("mcp__docs__search").map(|tool| tool.exposure()), Some(ToolExposure::Direct));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn a_changed_connection_config_invalidates_the_cache() {
    let dir = cached_dir(json!({})).await;
    let mut changed = docs_entry(json!({ "lifecycle": "lazy" }));
    changed.config.url = Some("http://other.invalid".into());
    assert_ne!(connection_fingerprint(&changed), connection_fingerprint(&docs_entry(json!({}))));
    assert!(read_cached_tools(dir.path(), &changed).is_none());
    // Settings that do not change the connection keep it.
    assert!(read_cached_tools(dir.path(), &docs_entry(json!({ "exposure": "direct", "timeout": 5 }))).is_some());
    let setup = setup_in(dir, Fake::new(FakeOptions::default()), loaded(vec![changed]), |_| {});
    assert!(setup.manager.tools().is_empty());
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn set_lifecycle_saves_it_and_eager_connects_at_once() {
    let patches: Arc<Mutex<Vec<McpServerConfigPatch>>> = Arc::default();
    let saved = patches.clone();
    let entries = loaded(vec![docs_entry(json!({ "lifecycle": "lazy" }))]);
    let setup = setup(Fake::new(FakeOptions::default()), entries, move |options| {
        options.update_config = Some(Arc::new(move |_, patch| {
            saved.lock().unwrap().push(*patch);
            Ok(())
        }));
    });
    assert_eq!(setup.manager.set_lifecycle("docs", McpLifecycle::Eager), None);
    let expected = McpServerConfigPatch { lifecycle: Some(McpLifecycle::Eager), ..McpServerConfigPatch::default() };
    assert_eq!(*patches.lock().unwrap(), [expected]);
    assert_eq!(setup.manager.status("docs").unwrap().lifecycle, McpLifecycle::Eager);
    wait_until(|| setup.manager.tool("mcp__docs__search").is_some()).await;
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn reconnect_connects_an_idle_lazy_server() {
    let setup = lazy(tempfile::tempdir().unwrap(), Fake::new(FakeOptions::default()), json!({}));
    assert_eq!(setup.manager.reconnect("docs").await, None);
    assert_eq!(setup.manager.status("docs").unwrap().state, Some(ServerState::Connected));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn acp_client_servers_connect_at_startup_and_are_not_cached() {
    let mut client = docs_entry(json!({ "lifecycle": "lazy" }));
    client.scope = Some(McpConfigScope::Client);
    let setup = setup(Fake::new(FakeOptions::default()), loaded(vec![client.clone()]), |_| {});
    setup.manager.pending().await;
    let status = setup.manager.status("docs").unwrap();
    assert_eq!((status.lifecycle, status.state), (McpLifecycle::Eager, Some(ServerState::Connected)));
    assert!(read_cached_tools(setup.dir.path(), &client).is_none());
    assert!(!setup.dir.path().join("mcp-cache").exists());
    setup.manager.shutdown().await;
}
