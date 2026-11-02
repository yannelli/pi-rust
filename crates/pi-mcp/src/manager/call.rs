//! Calls to a tool of one server by its MCP name, outside the agent's tool calls (the extension
//! method `mcp.callTool`). A call waits for a server that is connecting and never signs in.

use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::McpManager;
use crate::client::RequestOptions;
use crate::content::CallToolResult;
use crate::error::McpError;
use crate::runtime::ServerState;

#[derive(Debug, thiserror::Error)]
pub enum McpCallError {
    #[error("Unknown MCP server \"{0}\"")]
    UnknownServer(String),
    #[error("MCP server \"{server}\" has no tool \"{tool}\"")]
    UnknownTool { server: String, tool: String },
    /// Disabled, failed, closed, or waiting for a sign-in.
    #[error("{0}")]
    NotConnected(String),
    #[error("{0}")]
    Timeout(String),
    #[error("MCP tool call cancelled")]
    Cancelled,
    #[error(transparent)]
    Failed(McpError),
}

impl McpManager {
    /// Call `tool` of `server` with `args`. `timeout` bounds the wait for the connection and the call.
    pub async fn call_server_tool(
        &self,
        server: &str,
        tool: &str,
        args: Value,
        timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult, McpCallError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let timed_out =
            |what: &str| McpCallError::Timeout(format!("{what} {}ms", timeout.unwrap_or_default().as_millis()));
        let id = {
            let state = self.inner.lock();
            let found = state.named(server).ok_or_else(|| McpCallError::UnknownServer(server.into()))?;
            if !found.enabled() {
                return Err(McpCallError::NotConnected(format!("MCP server \"{server}\" is disabled")));
            }
            found.id
        };
        let ids = [id];
        let wait = self.inner.wait_for_servers(&ids, cancel);
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, wait)
                .await
                .map_err(|_| timed_out(&format!("MCP server \"{server}\" did not connect within")))?,
            None => wait.await,
        }
        if cancel.is_cancelled() {
            return Err(McpCallError::Cancelled);
        }
        let connection = self.connection(server);
        let connection = connection.ok_or_else(|| McpCallError::NotConnected(not_connected(server, None)))?;
        match connection.state() {
            ServerState::Connected | ServerState::Disconnected => {}
            ServerState::NeedsAuth => return Err(McpCallError::NotConnected(connection.sign_in_message())),
            _ => return Err(McpCallError::NotConnected(not_connected(server, connection.error()))),
        }
        if !connection.tools().iter().any(|listed| listed.name == tool) {
            return Err(McpCallError::UnknownTool { server: server.into(), tool: tool.into() });
        }
        let call_cancel = cancel.child_token();
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(timed_out("MCP tool call timed out after"));
        }
        let options = RequestOptions { cancel: Some(call_cancel.clone()), timeout: remaining, on_progress: None };
        let call = connection.call_tool(tool, args, options);
        tokio::pin!(call);
        let expired = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        let result = tokio::select! {
            result = &mut call => result,
            () = expired => {
                // Cancelling lets the client tell the server and drop the request.
                call_cancel.cancel();
                let _ = call.await;
                return Err(timed_out("MCP tool call timed out after"));
            }
        };
        result.map_err(|error| match error {
            _ if cancel.is_cancelled() => McpCallError::Cancelled,
            McpError::Timeout(_) if timeout.is_some() => timed_out("MCP tool call timed out after"),
            McpError::Timeout(ms) => McpCallError::Timeout(format!("MCP tool call timed out after {ms}ms")),
            error if connection.state() == ServerState::NeedsAuth => McpCallError::NotConnected(error.to_string()),
            error => McpCallError::Failed(error),
        })
    }
}

fn not_connected(server: &str, error: Option<String>) -> String {
    match error {
        Some(error) => format!("MCP server \"{server}\" is not connected: {error}"),
        None => format!("MCP server \"{server}\" is not connected"),
    }
}
