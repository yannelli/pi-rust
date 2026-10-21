//! Server states for lists, `/mcp` without a UI, and the startup report.

use std::path::Path;

use super::{Inner, McpManager, NotifyLevel, Server};
use crate::config::McpServerEntry;
use crate::resources::locale_cmp;
use crate::runtime::ServerState;

/// A server as lists show it.
#[derive(Clone, Debug)]
pub struct McpServerStatus {
    pub entry: McpServerEntry,
    pub enabled: bool,
    /// `None` until the connection is created.
    pub state: Option<ServerState>,
    pub error: Option<String>,
    pub tools: Vec<String>,
    /// Whether the server offers resources.
    pub has_resources: bool,
    /// Whether the server signs in with OAuth.
    pub oauth: bool,
    /// Result of the last action that failed.
    pub message: Option<String>,
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

fn plural(count: usize, word: &str) -> String {
    format!("{count} {word}{}", if count == 1 { "" } else { "s" })
}

/// Short state for lists and the startup report. `with_error` appends the first line of a failure.
pub fn describe_state(status: &McpServerStatus, with_error: bool) -> String {
    if !status.enabled {
        return "disabled".into();
    }
    match status.state {
        None => "starting".into(),
        Some(ServerState::NeedsAuth) => "needs sign-in".into(),
        Some(ServerState::Failed) if with_error => {
            format!("failed: {}", first_line(status.error.as_deref().unwrap_or("unknown error")))
        }
        Some(ServerState::Connected) => {
            let resources = if status.has_resources { " · resources" } else { "" };
            format!("connected · {}{resources}", plural(status.tools.len(), "tool"))
        }
        Some(ServerState::Connecting) => "connecting…".into(),
        Some(state) => state.to_string(),
    }
}

/// Servers that need the user come first.
pub(crate) fn attention_rank(status: &McpServerStatus) -> u8 {
    if !status.enabled {
        return 5;
    }
    match status.state {
        Some(ServerState::NeedsAuth) => 0,
        Some(ServerState::Failed) => 1,
        Some(ServerState::Disconnected) => 2,
        Some(ServerState::Connected) => 4,
        _ => 3,
    }
}

fn status_of(server: &Server) -> McpServerStatus {
    let connection = server.connection.as_ref();
    McpServerStatus {
        entry: server.entry.clone(),
        enabled: server.enabled(),
        state: connection.map(|connection| connection.state()),
        error: connection.and_then(|connection| connection.error()),
        tools: connection
            .map(|connection| connection.tools().into_iter().map(|tool| tool.name).collect())
            .unwrap_or_default(),
        has_resources: connection.is_some_and(|connection| connection.has_resources()),
        oauth: connection.is_some_and(|connection| connection.oauth_url().is_some()),
        message: server.message.clone(),
    }
}

pub(crate) fn no_servers_message(agent_dir: &Path) -> String {
    let path = std::path::absolute(agent_dir.join("mcp.json")).unwrap_or_else(|_| agent_dir.join("mcp.json"));
    format!("No MCP servers configured. Add them to {} or .pi/mcp.json.", path.display())
}

impl McpManager {
    /// Every server in config order.
    pub fn statuses(&self) -> Vec<McpServerStatus> {
        self.inner.lock().servers.iter().map(status_of).collect()
    }

    /// Servers sorted for a manager view: the ones that need the user first, then by name.
    pub fn statuses_by_attention(&self) -> Vec<McpServerStatus> {
        let mut statuses = self.statuses();
        statuses.sort_by(|a, b| {
            attention_rank(a).cmp(&attention_rank(b)).then_with(|| locale_cmp(&a.entry.name, &b.entry.name))
        });
        statuses
    }

    pub fn status(&self, name: &str) -> Option<McpServerStatus> {
        self.inner.lock().named(name).map(status_of)
    }

    pub fn config_errors(&self) -> Vec<String> {
        self.inner.lock().config_errors.clone()
    }

    /// The trusted project's `mcp.json`, where per-project overrides are saved.
    pub fn project_config(&self) -> Option<std::path::PathBuf> {
        self.inner.lock().project_config.clone()
    }

    /// The notice for a session without servers.
    pub fn no_servers_message(&self) -> String {
        no_servers_message(&self.inner.options.agent_dir)
    }

    /// Plain status for `/mcp` without a UI.
    pub fn format_status(&self) -> String {
        let (statuses, errors, overridden) = (self.statuses(), self.config_errors(), self.overridden());
        if statuses.is_empty() && errors.is_empty() && overridden.is_empty() {
            return no_servers_message(&self.inner.options.agent_dir);
        }
        let mut lines: Vec<String> = statuses
            .iter()
            .map(|status| {
                let name = &status.entry.name;
                let exposure = status.entry.config.exposure();
                if status.state == Some(ServerState::NeedsAuth) {
                    return format!("{name}: needs sign-in, run /mcp login {name} ({exposure})");
                }
                let connected = status.state == Some(ServerState::Connected);
                let tools = if connected { format!(", {} tools", status.tools.len()) } else { String::new() };
                let state = match status.state {
                    _ if !status.enabled => "disabled".to_string(),
                    Some(ServerState::Disconnected) => "disconnected, reconnects on next call".into(),
                    Some(state) => state.to_string(),
                    None => "starting".into(),
                };
                let error = match &status.error {
                    Some(error) if !connected => {
                        format!("\n    {}", error.split('\n').collect::<Vec<_>>().join("\n    "))
                    }
                    _ => String::new(),
                };
                format!("{name}: {state}{tools} ({exposure}){error}")
            })
            .collect();
        lines.extend(errors.iter().map(|error| format!("config error: {error}")));
        lines.extend(overridden.iter().map(|line| format!("overridden: {line}")));
        lines.join("\n")
    }
}

impl Inner {
    /// One message for everything that needs the user after startup, or only for `only`.
    pub(super) fn report_problems(&self, only: Option<&[u64]>) {
        let mut lines: Vec<String> = Vec::new();
        {
            let state = self.lock();
            if only.is_none() {
                lines.extend(state.config_errors.iter().map(|error| format!("config: {error}")));
            }
            let servers = state.servers.iter().filter(|server| only.is_none_or(|only| only.contains(&server.id)));
            for status in servers.map(status_of) {
                if matches!(status.state, Some(ServerState::NeedsAuth | ServerState::Failed)) {
                    lines.push(format!("{}: {}", status.entry.name, describe_state(&status, true)));
                }
            }
        }
        if lines.is_empty() {
            return;
        }
        let lines: Vec<String> = lines.iter().map(|line| format!("  {line}")).collect();
        let message = format!("MCP servers need attention:\n{}\nRun /mcp to fix.", lines.join("\n"));
        self.notify(&message, NotifyLevel::Warning);
    }
}
