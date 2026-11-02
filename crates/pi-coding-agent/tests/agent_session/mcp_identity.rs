//! `AgentSession::mcp_tool_identity` names the MCP server tool behind the tool object a call runs.
//! Resource tools and same-named tools of other sources get none, whatever the registry says later.

use pi_coding_agent::agent_session::ToolSource;
use serde_json::json;

use crate::mcp_fake::FakeOptions;
use crate::tool_registry::tool;
use crate::tool_support::{SetupOptions, setup};

const SEARCH: &str = "mcp__docs__search";

fn docs_search() -> Option<(String, String)> {
    Some(("docs".into(), "search".into()))
}

#[tokio::test]
async fn only_the_managers_own_server_tools_have_an_identity() {
    let fake = FakeOptions { resources: true, ..FakeOptions::default() };
    let s = setup(json!({ "exposure": "direct" }), SetupOptions { fake, ..SetupOptions::default() }).await;
    let session = &s.h.session;
    crate::harness::wait_until(|| session.tool("list_mcp_resources").is_some()).await;
    assert_eq!(session.mcp_tool_identity(&session.tool(SEARCH).unwrap()), docs_search());
    assert_eq!(session.mcp_tool_identity(&session.tool("list_mcp_resources").unwrap()), None);
    assert_eq!(session.mcp_tool_identity(&tool("mcp__docs__search", true)), None);
}

#[tokio::test]
async fn a_described_tool_has_the_identity_of_the_tool_it_runs() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions::default()).await;
    let session = &s.h.session;
    let mcp = session.tool(SEARCH).unwrap();
    let described = pi_agent::with_description(pi_agent::with_description(mcp, "first"), "second");
    assert_eq!(session.mcp_tool_identity(&described), docs_search());
    let shadow = pi_agent::with_description(tool("mcp__docs__search", true), "shadow");
    assert_eq!(session.mcp_tool_identity(&shadow), None);
}

#[tokio::test]
async fn a_shadow_tool_has_no_identity_after_the_registry_returns_to_mcp() {
    let s = setup(json!({ "exposure": "direct" }), SetupOptions::default()).await;
    let session = &s.h.session;
    let mcp = session.tool(SEARCH).unwrap();
    session.register_tool(tool("mcp__docs__search", true), ToolSource::extension("<inline:1>", "inline"));
    let shadow = session.tool(SEARCH).unwrap();
    // A call prepared now runs `shadow`; its hooks await while the MCP tool takes the name back.
    session.register_tool(mcp.clone(), ToolSource::builtin("mcp"));
    assert!(std::ptr::addr_eq(std::sync::Arc::as_ptr(&session.tool(SEARCH).unwrap()), std::sync::Arc::as_ptr(&mcp)));
    assert_eq!(session.mcp_tool_identity(&shadow), None);
    assert_eq!(session.mcp_tool_identity(&mcp), docs_search());
    // The reverse: a call prepared with the MCP tool still runs it after a shadow takes the name.
    session.register_tool(shadow.clone(), ToolSource::extension("<inline:1>", "inline"));
    assert_eq!(session.mcp_tool_identity(&mcp), docs_search());
    assert_eq!(session.mcp_tool_identity(&shadow), None);
}
