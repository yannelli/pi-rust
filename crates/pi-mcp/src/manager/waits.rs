//! Waiting for servers: the first prompt waits for servers with `direct` tools; tools that reach
//! other tools (codemode scripts, `tool_search`) and the resource tools wait for the servers they need
//! when they run.

use pi_agent::ToolReach;
use tokio_util::sync::CancellationToken;

use super::{Inner, McpManager, NotifyLevel, Ready};
use crate::config::mcp_namespace;
use crate::naming::{LIST_MCP_RESOURCE_TEMPLATES_TOOL, LIST_MCP_RESOURCES_TOOL, READ_MCP_RESOURCE_TOOL};
use crate::runtime::McpServerConnection;
use crate::section::{McpServerListing, has_direct_tools, render_servers_section};

pub const STILL_CONNECTING_MESSAGE: &str =
    "MCP servers are still connecting; their tools become available once connected.";

/// Whether a call that reaches `reach` (see `AgentTool::reaches_tools`) needs the server: it reaches
/// every tool, or one of its words contains the server's namespace.
pub fn reaches_server(reach: &ToolReach, server: &str) -> bool {
    match reach {
        ToolReach::All => true,
        ToolReach::Words(words) => {
            let namespace = mcp_namespace(server);
            words.iter().any(|word| word.contains(&namespace))
        }
    }
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

    /// Wait before a tool call for the servers it needs: those a tool that reaches other tools may
    /// reach (`reach`, from `AgentTool::reaches_tools`), all of them for the resource tools, else the
    /// tool's own server.
    pub async fn wait_for_tool_call(&self, tool_name: &str, reach: Option<&ToolReach>, cancel: &CancellationToken) {
        let waiting: Vec<u64> = {
            let state = self.inner.lock();
            let ready = state.servers.iter().filter(|server| server.enabled() && server.ready.is_some());
            let resource_tool = [LIST_MCP_RESOURCES_TOOL, LIST_MCP_RESOURCE_TEMPLATES_TOOL, READ_MCP_RESOURCE_TOOL]
                .contains(&tool_name);
            if let Some(reach) = reach {
                ready.filter(|server| reaches_server(reach, &server.entry.name)).map(|server| server.id).collect()
            } else if resource_tool {
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
    use pi_agent::ToolReach;

    use super::reaches_server;

    fn words(text: &[&str]) -> ToolReach {
        ToolReach::Words(text.iter().map(|word| word.to_string()).collect())
    }

    #[test]
    fn calls_need_the_servers_they_may_reach() {
        assert!(reaches_server(&words(&["await", "tools", "mcp__my_docs__search"]), "my-docs"));
        assert!(reaches_server(&ToolReach::All, "docs"));
        assert!(!reaches_server(&words(&["mysearchTools"]), "docs"));
        assert!(!reaches_server(&words(&["mcp__other__x"]), "docs"));
        assert!(!reaches_server(&words(&[]), "docs"));
    }
}
