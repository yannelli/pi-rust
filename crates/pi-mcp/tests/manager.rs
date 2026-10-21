//! The MCP manager: tool registration, exposure, resource tools, discovery, and the servers section.
//! Ported from coding-agent's agent-session-mcp.test.ts.

mod common;

use std::sync::Arc;

use common::fake_mcp::{Fake, FakeOptions, Setup, docs, server_tools};
use common::wait_until;
use pi_mcp::{JsonRpcMessage, McpAgentTool, McpManager, ToolAnnotations, ToolExposure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

async fn connected(setup: &Setup, tool: &str) {
    wait_until(|| setup.manager.tool(tool).is_some()).await;
}

fn exposures(manager: &McpManager) -> Vec<(String, ToolExposure)> {
    manager.tools().iter().map(|tool| (tool.name().to_string(), tool.exposure())).collect()
}

fn text_of(result: &pi_agent::AgentToolResult) -> String {
    let content = serde_json::to_value(&result.content).unwrap();
    let texts = content.as_array().unwrap().iter().filter_map(|block| block["text"].as_str().map(str::to_string));
    texts.collect::<Vec<_>>().join("\n")
}

async fn execute(tool: &McpAgentTool, args: Value) -> anyhow::Result<pi_agent::AgentToolResult> {
    tool.agent_tool().execute("call", args, CancellationToken::new(), Arc::new(|_| {})).await
}

#[tokio::test]
async fn declares_directly_exposed_tools_with_their_annotation_hints() {
    let setup = docs(json!({ "url": "http://unused.invalid", "exposure": "direct" }), FakeOptions::default());
    connected(&setup, "mcp__docs__shot").await;
    let names: Vec<_> = exposures(&setup.manager);
    let direct = |name: &str| (name.to_string(), ToolExposure::Direct);
    assert_eq!(names, [direct("mcp__docs__search"), direct("mcp__docs__fail"), direct("mcp__docs__shot")]);
    // Boolean annotation hints are passed on for permission extensions.
    let fail = setup.manager.tool("mcp__docs__fail").unwrap();
    let hints =
        ToolAnnotations { destructive_hint: Some(true), read_only_hint: Some(false), ..ToolAnnotations::default() };
    assert_eq!(fail.annotations(), Some(hints));
    let search = setup.manager.tool("mcp__docs__search").unwrap();
    assert_eq!(search.annotations(), None);
    assert_eq!(search.agent_tool().label(), "docs/search");
    assert_eq!(search.output_schema()["properties"]["structuredContent"]["required"], json!(["hits"]));
    let result = execute(&search, json!({ "query": "direct" })).await.unwrap();
    assert_eq!(text_of(&result), "direct guide\ndirect faq");
    assert_eq!(result.details, Some(json!({ "server": "docs", "tool": "search" })));
    // Scripts get the whole result.
    let output = search.call(json!({ "query": "q" }), CancellationToken::new(), None).await.unwrap();
    assert_eq!(output.structured_content["structuredContent"], json!({ "hits": ["q guide", "q faq"] }));
    let failed = execute(&fail, json!({})).await.unwrap();
    assert!(failed.is_error);
    assert_eq!(text_of(&failed), "server exploded");
    assert_eq!(setup.fake.calls(), [r#"search:{"query":"direct"}"#, r#"search:{"query":"q"}"#, "fail:{}"]);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn withdraws_and_restores_tools_the_server_changes() {
    let setup = docs(json!({ "url": "http://unused.invalid", "exposure": "direct" }), FakeOptions::default());
    connected(&setup, "mcp__docs__fail").await;
    let exposure_of = |name: &str| setup.manager.tool(name).map(|tool| tool.exposure());
    let list_changed = || async {
        setup.fake.server(0).send(JsonRpcMessage::notification("notifications/tools/list_changed", None)).await;
    };
    let without_fail: Vec<Value> =
        server_tools().as_array().unwrap().iter().filter(|tool| tool["name"] != "fail").cloned().collect();
    *setup.fake.tools.lock().unwrap() = Value::Array(without_fail);
    list_changed().await;
    wait_until(|| exposure_of("mcp__docs__fail") == Some(ToolExposure::Hidden)).await;
    assert_eq!(exposure_of("mcp__docs__search"), Some(ToolExposure::Direct));
    let error = execute(&setup.manager.tool("mcp__docs__fail").unwrap(), json!({})).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP tool \"docs/fail\" is no longer available.");

    *setup.fake.tools.lock().unwrap() = server_tools();
    list_changed().await;
    wait_until(|| exposure_of("mcp__docs__fail") == Some(ToolExposure::Direct)).await;
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn lists_and_reads_resources_with_codexs_resource_tools() {
    let options = FakeOptions { resources: true, ..FakeOptions::default() };
    let setup = docs(json!({ "url": "http://unused.invalid", "exposure": "direct" }), options);
    connected(&setup, "read_mcp_resource").await;
    // The resource tools take the exposure of the servers they reach.
    let read = setup.manager.tool("read_mcp_resource").unwrap();
    assert_eq!(read.exposure(), ToolExposure::Direct);
    assert_eq!(read.annotations(), Some(ToolAnnotations { read_only_hint: Some(true), ..ToolAnnotations::default() }));
    let list = setup.manager.tool("list_mcp_resources").unwrap();
    let listed: Value = serde_json::from_str(&text_of(&execute(&list, json!({})).await.unwrap())).unwrap();
    assert_eq!(
        listed,
        json!({ "resources": [{ "server": "docs", "uri": "docs://intro", "name": "intro", "mimeType": "text/markdown" }] })
    );
    let templates = setup.manager.tool("list_mcp_resource_templates").unwrap();
    let result = execute(&templates, json!({ "server": "docs" })).await.unwrap();
    let listed: Value = serde_json::from_str(&text_of(&result)).unwrap();
    let expected = json!({ "server": "docs", "resourceTemplates": [{ "server": "docs", "uriTemplate": "docs://pages/{slug}", "name": "page" }] });
    assert_eq!(listed, expected);
    let page = execute(&read, json!({ "server": "docs", "uri": "docs://pages/setup" })).await.unwrap();
    assert_eq!(text_of(&page), "# docs://pages/setup");
    let error = execute(&read, json!({ "server": "nope", "uri": "docs://x" })).await.unwrap_err();
    assert_eq!(error.to_string(), "MCP server \"nope\" has no resources. Servers with resources: docs");
    let error = execute(&list, json!({ "cursor": "c1" })).await.unwrap_err();
    assert_eq!(error.to_string(), "cursor can only be used when a server is specified");
    // Scripts get the payload.
    let output =
        read.call(json!({ "server": "docs", "uri": "docs://intro" }), CancellationToken::new(), None).await.unwrap();
    let contents = json!([{ "uri": "docs://intro", "mimeType": "text/markdown", "text": "# docs://intro" }]);
    assert_eq!(output.structured_content, json!({ "server": "docs", "uri": "docs://intro", "contents": contents }));
    assert_eq!(setup.fake.calls(), ["read:docs://pages/setup", "read:docs://intro"]);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn applies_per_tool_exposure_overrides() {
    let config = json!({ "url": "http://unused.invalid", "exposure": "hidden", "toolExposure": { "search": "direct", "s*": "codemode" } });
    let setup = docs(config, FakeOptions::default());
    connected(&setup, "mcp__docs__shot").await;
    // `search` is declared, `shot` is reached through codemode, `fail` keeps the server's `hidden`.
    let expected = [
        ("mcp__docs__search".to_string(), ToolExposure::Direct),
        ("mcp__docs__fail".to_string(), ToolExposure::Hidden),
        ("mcp__docs__shot".to_string(), ToolExposure::Deferred),
    ];
    assert_eq!(exposures(&setup.manager), expected);
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn describes_the_server_with_its_description_and_instructions() {
    let options = FakeOptions { instructions: Some("Always search before reading.".into()), ..FakeOptions::default() };
    let setup = docs(json!({ "url": "http://unused.invalid", "description": "Search the product docs" }), options);
    connected(&setup, "mcp__docs__search").await;
    let tool = setup.manager.tool("mcp__docs__search").unwrap();
    let namespace = tool.namespace().unwrap();
    assert_eq!(namespace.name, "mcp__docs");
    // `describeNamespace("docs")` finds `mcp__docs`.
    assert_eq!(namespace.aliases, ["docs"]);
    assert_eq!(namespace.description.as_deref(), Some("Search the product docs"));
    assert_eq!(namespace.instructions.as_deref(), Some("Always search before reading."));
    assert_eq!(tool.exposure(), ToolExposure::Deferred);
    // The system prompt lists the server with its configured description rather than its instructions.
    let section = setup.manager.servers_section().unwrap();
    assert!(section.contains("- mcp__docs (codemode): Search the product docs"), "{section}");
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn lists_servers_by_the_first_line_of_their_instructions_without_a_description() {
    let options = FakeOptions { instructions: Some("Docs search.\nLong guidance.".into()), ..FakeOptions::default() };
    let setup = docs(json!({ "url": "http://unused.invalid", "exposure": "deferred" }), options);
    // Before it connects, the server is listed by name.
    let section = setup.manager.servers_section().unwrap();
    assert!(section.ends_with("- mcp__docs (tool_search)"), "{section}");
    connected(&setup, "mcp__docs__search").await;
    let section = setup.manager.servers_section().unwrap();
    assert!(section.ends_with("- mcp__docs (tool_search): Docs search."), "{section}");
    assert!(!section.contains("Long guidance"));
    setup.manager.shutdown().await;
}

#[tokio::test]
async fn activates_codemode_or_tool_search_for_the_exposures_in_use() {
    let codemode = docs(json!({ "url": "http://unused.invalid" }), FakeOptions::default());
    assert_eq!(codemode.manager.ensure_discovery_active(true, true, &[]), ["codemode"]);
    // An active tool_search reaches codemode tools too.
    assert!(codemode.manager.ensure_discovery_active(false, true, &["tool_search".into()]).is_empty());
    codemode.manager.shutdown().await;
    assert!(codemode.notifications.lock().unwrap().is_empty());

    let deferred = docs(json!({ "url": "http://unused.invalid", "exposure": "deferred" }), FakeOptions::default());
    assert_eq!(deferred.manager.ensure_discovery_active(true, true, &[]), ["tool_search"]);
    // Deferred tools are reachable through codemode when tool_search is not available.
    assert!(deferred.manager.ensure_discovery_active(true, false, &["codemode".into()]).is_empty());
    assert!(deferred.notifications.lock().unwrap().is_empty());
    assert!(deferred.manager.ensure_discovery_active(false, false, &[]).is_empty());
    assert!(deferred.manager.ensure_discovery_active(false, false, &[]).is_empty());
    let warning = "MCP tools are only reachable from the codemode or tool_search tool, but neither is active; they cannot be called.";
    assert_eq!(*deferred.notifications.lock().unwrap(), [warning]);
    deferred.manager.shutdown().await;
}

#[tokio::test]
async fn does_not_activate_codemode_when_auto_enable_codemode_is_false() {
    let loaded = pi_mcp::LoadedMcpConfig {
        servers: vec![common::fake_mcp::entry("docs", json!({ "url": "http://unused.invalid" }))],
        auto_enable_codemode: Some(false),
        ..Default::default()
    };
    let setup = common::fake_mcp::setup(Fake::new(FakeOptions::default()), loaded, |_| {});
    assert!(setup.manager.ensure_discovery_active(true, true, &[]).is_empty());
    let warning = "MCP tools are only reachable from the codemode or tool_search tool, but neither is active (autoEnableCodemode is false); they cannot be called.";
    assert_eq!(*setup.notifications.lock().unwrap(), [warning]);
    connected(&setup, "mcp__docs__search").await;
    setup.manager.shutdown().await;
}
