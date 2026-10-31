//! The MCP servers that ACP agents reach through pi's proxy: enabled HTTP servers whose login pi
//! holds (OAuth or `auth.provider`), which pi does not pass through. pi calls their tools itself.

use std::time::Duration;

use pi_agent::AgentTool;
use pi_ai::ContentBlock;
use pi_ai::providers::acp::{McpProxy, ProxyTool};
use pi_mcp::manager::McpAgentTool;
use pi_mcp::{McpExposure, McpManager, McpServerEntry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// How long the first ACP session waits for those servers to connect.
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// Whether the agent reaches `entry` through the proxy rather than by itself.
pub fn proxied(entry: &McpServerEntry) -> bool {
    let config = &entry.config;
    config.is_enabled()
        && config.exposure() != McpExposure::Hidden
        && config.is_http()
        && (config.uses_oauth() || config.auth_provider().is_some())
}

/// The proxied servers of `manager` and their tools that are not hidden.
pub async fn offer(manager: &McpManager) -> McpProxy {
    let _ = tokio::time::timeout(CONNECT_WAIT, manager.pending()).await;
    let servers: Vec<String> = manager.entries().into_iter().filter(proxied).map(|entry| entry.name).collect();
    let tools = manager.tools().into_iter().filter_map(|tool| match tool {
        McpAgentTool::Tool(tool) if servers.iter().any(|server| server == tool.server()) => Some(tool),
        _ => None,
    });
    let tools = tools.filter(|tool| tool.exposure() != pi_agent::ToolExposure::Hidden);
    let tools = tools.map(|tool| ProxyTool {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        schema: tool.parameters(),
    });
    McpProxy { tools: tools.collect(), servers }
}

fn error(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": true})
}

/// Calls proxied tool `name` and returns an MCP `CallToolResult`.
pub async fn call(manager: &McpManager, name: &str, arguments: Value) -> Value {
    let tool = manager.tool(name).filter(|tool| match tool {
        McpAgentTool::Tool(tool) => manager.entries().iter().any(|entry| entry.name == tool.server() && proxied(entry)),
        McpAgentTool::Resource(_) => false,
    });
    let Some(tool) = tool else { return error(&format!("pi does not offer the MCP tool {name}")) };
    let output = match tool.call(arguments, CancellationToken::new(), None).await {
        Ok(output) => output,
        Err(failure) => return error(&format!("{failure:#}")),
    };
    let content = output.content.iter().filter_map(|block| match block {
        ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
        ContentBlock::Image { data, mime_type } => Some(json!({"type": "image", "data": data, "mimeType": mime_type})),
        _ => None,
    });
    let mut result = json!({"content": content.collect::<Vec<_>>(), "isError": output.is_error});
    if output.structured_content.is_object() {
        result["structuredContent"] = output.structured_content.clone();
    }
    result
}
