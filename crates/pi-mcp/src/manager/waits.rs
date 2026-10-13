//! Waiting for servers: the first prompt waits for servers with `direct` tools; codemode scripts,
//! `tool_search`, and the resource tools wait for the servers they need when they run.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{CODEMODE_TOOL_NAME, Inner, McpManager, NotifyLevel, Ready, TOOL_SEARCH_TOOL_NAME};
use crate::config::mcp_namespace;
use crate::naming::{LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, READ_MCP_RESOURCE_TOOL};
use crate::runtime::McpServerConnection;
use crate::section::{McpServerListing, has_direct_tools, render_servers_section};

pub const STILL_CONNECTING_MESSAGE: &str =
    "MCP servers are still connecting; their tools become available once connected.";

/// Whether a codemode script needs the server: it names its namespace, or searches, enumerates, or
/// describes tools, which may name the server in other forms.
pub fn script_needs_server(code: &str, server: &str) -> bool {
    let word =
        |character: Option<char>| character.is_some_and(|character| character.is_alphanumeric() || character == '_');
    let names_global = ["searchTools", "describeNamespace", "describeTool", "ALL_TOOLS"].iter().any(|global| {
        code.match_indices(global).any(|(start, _)| {
            !word(code[..start].chars().next_back()) && !word(code[start + global.len()..].chars().next())
        })
    });
    names_global || code.contains(&mcp_namespace(server))
}

impl McpManager {
    /// The first prompt waits for servers with `direct` tools, up to `startup_wait`, once per session.
    /// Returns false when it stopped waiting; their tools register once they connect.
    pub async fn wait_for_direct_servers(&self) -> bool {
        let ready: Vec<Ready> = {
            let mut state = self.inner.lock();
            if state.waited_for_startup {
                return true;
            }
            state.waited_for_startup = true;
            let waiting = state.servers.iter().filter(|server| server.enabled() && has_direct_tools(&server.entry));
            waiting.filter_map(|server| server.ready.clone()).collect()
        };
        if ready.is_empty() {
            return true;
        }
        let all = futures::future::join_all(ready);
        let finished = tokio::time::timeout(self.inner.options.startup_wait, all).await.is_ok();
        if !finished {
            self.inner.notify(STILL_CONNECTING_MESSAGE, NotifyLevel::Info);
        }
        finished
    }

    /// The `mcp_servers` system prompt section as the servers are now.
    pub fn servers_section(&self) -> Option<String> {
        let state = self.inner.lock();
        let listings: Vec<McpServerListing> = state
            .servers
            .iter()
            .map(|server| McpServerListing {
                entry: server.entry.clone(),
                instructions: server.connection.as_ref().and_then(McpServerConnection::instructions),
            })
            .collect();
        render_servers_section(&listings)
    }

    /// Wait before a tool call for the servers it needs: those a codemode script names, all of them
    /// for `tool_search` and the resource tools, else the tool's own server.
    pub async fn wait_for_tool_call(&self, tool_name: &str, input: &Value, cancel: &CancellationToken) {
        let waiting: Vec<u64> = {
            let state = self.inner.lock();
            let ready = state.servers.iter().filter(|server| server.enabled() && server.ready.is_some());
            let resource_tool = [LIST_MCP_RESOURCES_TOOL, LIST_MCP_RESOURCE_TEMPLATES_TOOL, READ_MCP_RESOURCE_TOOL]
                .contains(&tool_name);
            if tool_name == CODEMODE_TOOL_NAME {
                let code = input.get("code").and_then(Value::as_str).unwrap_or_default();
                ready.filter(|server| script_needs_server(code, &server.entry.name)).map(|server| server.id).collect()
            } else if tool_name == TOOL_SEARCH_TOOL_NAME || resource_tool {
                ready.map(|server| server.id).collect()
            } else {
                let owner = state.registry.owner(tool_name);
                let owned = ready.filter(|server| owner.as_deref() == Some(server.entry.name.as_str()));
                owned.map(|server| server.id).collect()
            }
        };
        self.inner.wait_for_servers(&waiting, cancel).await;
    }
}

impl Inner {
    /// Wait for the latest connection tries of the servers, including ones queued while waiting.
    pub(super) async fn wait_for_servers(&self, ids: &[u64], cancel: &CancellationToken) {
        loop {
            let ready: Vec<(u64, Ready)> = {
                let state = self.lock();
                let servers = ids.iter().filter_map(|id| state.server(*id)).filter(|server| server.enabled());
                servers.filter_map(|server| Some((server.id, server.ready.clone()?))).collect()
            };
            if ready.is_empty() || cancel.is_cancelled() {
                return;
            }
            tokio::select! {
                _ = futures::future::join_all(ready.iter().map(|(_, ready)| ready.clone())) => {}
                () = cancel.cancelled() => return,
            }
            let state = self.lock();
            let settled = ready.iter().all(|(id, ready)| match state.server(*id) {
                Some(server) => !server.enabled() || server.ready.as_ref().is_some_and(|current| current.ptr_eq(ready)),
                None => true,
            });
            if settled {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::script_needs_server;

    #[test]
    fn scripts_need_the_servers_they_name_or_search() {
        assert!(script_needs_server("await tools.mcp__my_docs__search({})", "my-docs"));
        assert!(script_needs_server("const found = await searchTools('x')", "docs"));
        assert!(script_needs_server("return ALL_TOOLS.length", "docs"));
        assert!(!script_needs_server("mysearchTools()", "docs"));
        assert!(!script_needs_server("await tools.mcp__other__x()", "docs"));
    }
}
