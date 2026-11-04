//! `lazy` servers: at startup they register the tools of their tool cache, and they connect when
//! one of their tools is called, when `tool_search` or a codemode script needs a server without a
//! tool cache, or when the user connects them.

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{Inner, McpManager, Server};
use crate::client::RequestOptions;
use crate::config::{McpLifecycle, McpServerConfigPatch};
use crate::content::CallToolResult;
use crate::error::McpError;
use crate::runtime::{McpServerConnection, ServerState, request_timeout};
use crate::tool_cache::read_cached_tools;
use crate::tools::{McpToolCaller, number};

/// The connection when it can take calls, else why it cannot.
pub(super) fn usable(server: &str, connection: &McpServerConnection) -> Result<(), String> {
    match connection.state() {
        ServerState::Connected | ServerState::Disconnected => Ok(()),
        ServerState::NeedsAuth => Err(connection.sign_in_message()),
        _ => {
            let error = connection.error().unwrap_or_else(|| "unknown error".into());
            Err(format!("MCP server \"{server}\" failed to connect: {error}"))
        }
    }
}

/// Calls a tool of a `lazy` server that has not connected: connects it first.
pub(super) struct ConnectOnCall {
    inner: Weak<Inner>,
    server: String,
}

impl ConnectOnCall {
    pub(super) fn new(inner: Weak<Inner>, server: &str) -> Self {
        Self { inner, server: server.into() }
    }
}

#[async_trait]
impl McpToolCaller for ConnectOnCall {
    async fn call_tool(&self, name: &str, args: Value, options: RequestOptions) -> Result<CallToolResult, McpError> {
        let server = &self.server;
        let inner =
            self.inner.upgrade().ok_or_else(|| McpError::other(format!("MCP server \"{server}\" is disabled.")))?;
        let cancel = options.cancel.clone().unwrap_or_default();
        let connection = inner.connect_for_call(server, &cancel).await.map_err(McpError::other)?;
        drop(inner);
        if !connection.tools().iter().any(|tool| tool.name == name) {
            return Err(McpError::other(format!("MCP tool \"{server}/{name}\" is no longer available.")));
        }
        connection.call_tool(name, args, options).await
    }
}

impl Inner {
    /// Register the tools of the server's tool cache, when it has a valid one.
    pub(super) fn register_cached_tools(self: &Arc<Self>, id: u64) {
        let Some(entry) = self.lock().server(id).map(|server| server.entry.clone()) else { return };
        let Some(cached) = read_cached_tools(&self.options.agent_dir, &entry) else { return };
        if let Some(server) = self.lock().server_mut(id) {
            server.cached = true;
            server.cached_instructions = cached.instructions.clone();
        }
        self.register_offered(&entry.name, cached.tools, cached.instructions, request_timeout(&entry.config));
    }

    /// Start connecting the server when it is idle (see `State::is_idle`), after the close of its
    /// last connection. Other servers are left as they are.
    pub(super) fn connect_idle(self: &Arc<Self>, id: u64) {
        let closing = {
            let state = self.lock();
            let Some(server) = state.server(id).filter(|server| state.is_idle(server)) else { return };
            server.closing.clone()
        };
        // The connection runs in its own task; callers wait through `ready`.
        drop(self.start_connection(id, closing));
    }

    /// Idle servers without a tool cache, of those `include` selects: their tools are unknown until
    /// they connect.
    pub(super) fn idle_without_tools(&self, include: impl Fn(&Server) -> bool) -> Vec<u64> {
        let state = self.lock();
        let idle = state.servers.iter().filter(|server| state.is_idle(server) && !server.cached && include(server));
        idle.map(|server| server.id).collect()
    }

    /// Connect a server for a tool call and wait for it, up to the server's `timeout`.
    async fn connect_for_call(
        self: &Arc<Self>,
        server: &str,
        cancel: &CancellationToken,
    ) -> Result<McpServerConnection, String> {
        let disabled = || format!("MCP server \"{server}\" is disabled.");
        let (id, timeout) = {
            let state = self.lock();
            let found = state.named(server).filter(|found| found.enabled()).ok_or_else(disabled)?;
            (found.id, request_timeout(&found.entry.config))
        };
        self.connect_idle(id);
        if tokio::time::timeout(timeout, self.wait_for_servers(&[id], cancel)).await.is_err() {
            return Err(format!("MCP server \"{server}\" did not connect within {}s.", number(timeout.as_secs_f64())));
        }
        if cancel.is_cancelled() {
            return Err("MCP tool call cancelled".into());
        }
        let connection = self.lock().server(id).and_then(|found| found.connection.clone()).ok_or_else(disabled)?;
        usable(server, &connection)?;
        Ok(connection)
    }
}

impl McpManager {
    /// Change when a server connects, saved to its config. A server set to `eager` connects now.
    pub fn set_lifecycle(&self, name: &str, lifecycle: McpLifecycle) -> Option<String> {
        let patch = McpServerConfigPatch { lifecycle: Some(lifecycle), ..McpServerConfigPatch::default() };
        if let Some(failed) = self.inner.save_config(name, &patch, false) {
            return Some(failed);
        }
        let start = {
            let state = self.inner.lock();
            let server = state.named(name).filter(|server| {
                server.enabled() && server.attempt.is_none() && state.lifecycle(server) == McpLifecycle::Eager
            });
            server.map(|server| (server.id, server.closing.clone()))
        };
        if let Some((id, closing)) = start {
            drop(self.inner.start_connection(id, closing));
        }
        self.inner.emit_change();
        None
    }
}
