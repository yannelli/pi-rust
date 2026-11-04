//! `McpManager::call_server_tool`: calls a tool of one server by its MCP name, for extensions.

use crate::common;

use std::time::Duration;

use common::fake_mcp::{Fake, FakeOptions, docs, entry, setup};
use pi_mcp::{LoadedMcpConfig, McpCallError, McpError};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn slow(delay: Duration) -> common::fake_mcp::Setup {
    let loaded = LoadedMcpConfig {
        servers: vec![entry("slow", json!({ "url": "http://unused.invalid" }))],
        ..Default::default()
    };
    setup(Fake::new(FakeOptions { initialize_delay: Some(delay), ..FakeOptions::default() }), loaded, |_| {})
}

#[tokio::test]
async fn calls_a_tool_by_its_mcp_name_and_keeps_error_results() {
    let setup = docs(json!({ "url": "http://unused.invalid" }), FakeOptions::default());
    let cancel = CancellationToken::new();
    let result =
        setup.manager.call_server_tool("docs", "search", json!({ "query": "q" }), None, &cancel).await.unwrap();
    let value = serde_json::to_value(&result).unwrap();
    assert_eq!(value["content"], json!([{ "type": "text", "text": "q guide\nq faq" }]));
    assert_eq!(value["structuredContent"], json!({ "hits": ["q guide", "q faq"] }));
    assert_eq!(result.is_error, None);

    let failed = setup.manager.call_server_tool("docs", "fail", json!({}), None, &cancel).await.unwrap();
    assert_eq!(failed.is_error, Some(true));
    assert_eq!(setup.fake.calls(), [r#"search:{"query":"q"}"#, "fail:{}"]);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn refuses_unknown_and_disabled_servers_and_unknown_tools() {
    let servers = vec![
        entry("docs", json!({ "url": "http://unused.invalid" })),
        entry("off", json!({ "url": "http://unused.invalid", "enabled": false })),
    ];
    let setup = setup(Fake::new(FakeOptions::default()), LoadedMcpConfig { servers, ..Default::default() }, |_| {});
    let cancel = CancellationToken::new();
    let call = |server: &'static str, tool: &'static str| {
        let (manager, cancel) = (setup.manager.clone(), cancel.clone());
        async move { manager.call_server_tool(server, tool, json!({}), None, &cancel).await.unwrap_err() }
    };
    assert!(matches!(call("nope", "search").await, McpCallError::UnknownServer(name) if name == "nope"));
    let error = call("docs", "nope").await;
    assert_eq!(error.to_string(), "MCP server \"docs\" has no tool \"nope\"");
    assert!(matches!(call("off", "search").await, McpCallError::NotConnected(message) if message.contains("disabled")));
    assert!(setup.fake.calls().is_empty());
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn waits_for_a_connecting_server_up_to_the_timeout() {
    let setup = slow(Duration::from_millis(30));
    let cancel = CancellationToken::new();
    let result = setup.manager.call_server_tool("slow", "search", json!({ "query": "a" }), None, &cancel).await;
    assert_eq!(result.unwrap().is_error, None);
    setup.manager.shutdown().await;

    let never = slow(Duration::MAX);
    let timeout = Some(Duration::from_millis(40));
    let error = never.manager.call_server_tool("slow", "search", json!({}), timeout, &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP server \"slow\" did not connect within 40ms");
    never.manager.shutdown().await;
}

#[tokio::test]
async fn a_call_stops_at_the_timeout_or_when_cancelled() {
    let setup = docs(json!({ "url": "http://unused.invalid" }), FakeOptions::default());
    let cancel = CancellationToken::new();
    setup.manager.call_server_tool("docs", "search", json!({ "query": "a" }), None, &cancel).await.unwrap();
    setup.fake.server(0).set_handler("tools/call", |_| futures::future::pending::<Result<Value, McpError>>());
    let timeout = Some(Duration::from_millis(40));
    let error = setup.manager.call_server_tool("docs", "search", json!({}), timeout, &cancel).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP tool call timed out after 40ms");

    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        stop.cancel();
    });
    let error = setup.manager.call_server_tool("docs", "search", json!({}), None, &cancel).await.unwrap_err();
    assert!(matches!(error, McpCallError::Cancelled), "{error}");
    setup.manager.shutdown().await;
}
