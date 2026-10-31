//! The loopback MCP server through which an agent reaches the MCP servers whose login pi holds
//! (OAuth or a pi provider key). pi calls their tools with its own MCP connections.

use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::host::{McpProxy, host};
use crate::providers::antigravity_acp::{Bridge, BridgeTool, ParkedCall};

/// The name of the proxy server in `mcpServers`.
pub(crate) const PROXY_NAME: &str = "pi-mcp";

/// A running proxy; dropping it stops the server and its calls.
pub(crate) struct Proxy {
    bridge: Bridge,
    calls: JoinHandle<()>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.calls.abort();
    }
}

fn bridge_tools(offered: McpProxy) -> Vec<BridgeTool> {
    let tools = offered.tools.into_iter();
    tools.map(|tool| BridgeTool { name: tool.name, description: tool.description, schema: tool.schema }).collect()
}

/// Starts the proxy when the host offers servers, with the `mcpServers` entry for it, or the
/// notice why the agent does not get them.
pub(crate) async fn start(
    capabilities: &Value,
    agent_name: &str,
    pi_session: &str,
) -> Result<Option<(Value, Proxy)>, String> {
    let Some(host) = host() else { return Ok(None) };
    let offered = host.mcp_proxy(pi_session).await;
    if offered.servers.is_empty() {
        return Ok(None);
    }
    let names = offered.servers.join("\", \"");
    if capabilities.pointer("/mcpCapabilities/http") != Some(&Value::Bool(true)) {
        return Err(format!(
            "{agent_name} does not take http MCP servers, so it does not get \"{names}\", whose login pi holds."
        ));
    }
    let (sender, mut parked) = mpsc::unbounded_channel::<ParkedCall>();
    let bridge = Bridge::start(sender)
        .await
        .map_err(|error| format!("pi could not start its MCP proxy for \"{names}\": {error}"))?;
    bridge.set_tools(bridge_tools(offered));
    let session = pi_session.to_string();
    let calls = tokio::spawn(async move {
        while let Some(call) = parked.recv().await {
            let (host, session) = (host.clone(), session.clone());
            tokio::spawn(async move {
                let result = host.call_mcp_proxy(&session, &call.tool, Value::Object(call.arguments)).await;
                let _ = call.reply.send(result);
            });
        }
    });
    let authorization = format!("Bearer {}", bridge.token);
    let server = json!({"type": "http", "name": PROXY_NAME, "url": bridge.url,
        "headers": [{"name": "Authorization", "value": authorization}]});
    Ok(Some((server, Proxy { bridge, calls })))
}

impl Proxy {
    /// Lists the tools the host offers now, for servers that connected after the session started.
    pub async fn refresh(&self, pi_session: &str) {
        if let Some(host) = host() {
            self.bridge.set_tools(bridge_tools(host.mcp_proxy(pi_session).await));
        }
    }
}
