//! MCP actions for the `/mcp` views and RPC (upstream `extensions/mcp/index.ts` manager actions):
//! status, sign in and out, reconnect, enable or disable (also per project), and exposure. Each
//! returns the message to show when it failed, and activates codemode or `tool_search` as needed.

use std::sync::Arc;

use pi_mcp::manager::describe_state;
use pi_mcp::runtime::{McpSignInPrompt, ServerState};
use pi_mcp::{McpExposure, McpServerStatus, McpSubscription, NotifyLevel};
use tokio_util::sync::CancellationToken;

use crate::agent_session::{AgentSession, lock};

const MCP_USAGE: &str = "Usage: /mcp, /mcp login [server], /mcp logout [server], /mcp reconnect [server]";
const MCP_DISABLED: &str = "Built-in MCP support is disabled.";

impl AgentSession {
    /// Wait until the connections started with the session connected or failed.
    pub async fn mcp_pending(&self) {
        if let Some(manager) = self.mcp_manager() {
            manager.pending().await;
        }
    }

    /// Servers for a manager view: the ones that need the user first, then by name.
    pub fn mcp_statuses(&self) -> Vec<McpServerStatus> {
        self.mcp_manager().map(|manager| manager.statuses_by_attention()).unwrap_or_default()
    }

    /// Config problems found when the session started.
    pub fn mcp_config_errors(&self) -> Vec<String> {
        self.mcp_manager().map(|manager| manager.config_errors()).unwrap_or_default()
    }

    /// Plain server status, as `/mcp` shows it without a UI.
    pub fn mcp_status_text(&self) -> String {
        self.mcp_manager().map_or_else(|| MCP_DISABLED.to_string(), |manager| manager.format_status())
    }

    /// Whether a global server can be turned on or off for this project alone: the project is
    /// trusted and the server has no project override yet.
    pub fn mcp_can_save_in_project(&self, name: &str) -> bool {
        let Some(status) = self.mcp_manager().and_then(|manager| manager.status(name)) else { return false };
        status.entry.scope == Some(pi_mcp::McpConfigScope::Global)
            && status.entry.override_path.is_none()
            && self.settings().is_project_trusted()
    }

    /// Called whenever servers, their states, or their tools change, until `unsubscribe_mcp`.
    /// `None` when MCP is off.
    pub fn subscribe_mcp(&self, listener: impl Fn() + Send + Sync + 'static) -> Option<McpSubscription> {
        self.mcp_manager().map(|manager| manager.subscribe(listener))
    }

    pub fn unsubscribe_mcp(&self, subscription: McpSubscription) {
        if let Some(manager) = self.mcp_manager() {
            manager.unsubscribe(subscription);
        }
    }

    /// Sign in with the browser flow, then reconnect. `cancel` stops the sign-in.
    pub async fn mcp_sign_in(
        &self,
        name: &str,
        prompt: Arc<dyn McpSignInPrompt>,
        cancel: Option<CancellationToken>,
    ) -> Option<String> {
        let Some(manager) = self.mcp_manager() else { return Some(MCP_DISABLED.into()) };
        let failure = manager.sign_in(name, prompt, cancel).await;
        self.ensure_mcp_discovery();
        failure
    }

    /// Delete the stored credentials and disconnect. Returns whether credentials were stored.
    pub async fn mcp_sign_out(&self, name: &str) -> bool {
        match self.mcp_manager() {
            Some(manager) => manager.sign_out(name).await,
            None => false,
        }
    }

    pub async fn mcp_reconnect(&self, name: &str) -> Option<String> {
        let Some(manager) = self.mcp_manager() else { return Some(MCP_DISABLED.into()) };
        let failure = manager.reconnect(name).await;
        self.ensure_mcp_discovery();
        failure
    }

    /// Enable or disable a server, saved to its `mcp.json`; `in_project` saves a project override.
    pub async fn mcp_set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String> {
        let Some(manager) = self.mcp_manager() else { return Some(MCP_DISABLED.into()) };
        let failure = manager.set_enabled(name, enabled, in_project).await;
        manager.set_message(name, failure.clone());
        self.ensure_mcp_discovery();
        failure
    }

    /// Change a server's exposure, saved to its `mcp.json`. Tools that stop being `direct` leave
    /// the declared set.
    pub fn mcp_set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String> {
        let Some(manager) = self.mcp_manager() else { return Some(MCP_DISABLED.into()) };
        let failure = manager.set_exposure(name, exposure);
        self.ensure_mcp_discovery();
        failure
    }

    /// The prompt `/mcp login` signs in with, such as one that opens the browser and reads a
    /// pasted redirect URL. Without one, `/mcp login` needs an interactive mode.
    pub fn set_mcp_sign_in_prompt(&self, prompt: Option<Arc<dyn McpSignInPrompt>>) {
        lock(&self.builtins).sign_in_prompt = prompt;
    }

    /// Handle `/mcp [login|logout|reconnect] [server]` without a manager view (upstream `/mcp`
    /// command outside the TUI). Results go to the notify handler.
    pub async fn run_mcp_command(&self, args: &str) {
        let Some(manager) = self.mcp_manager() else {
            return self.notify(MCP_DISABLED, NotifyLevel::Info);
        };
        let words: Vec<&str> = args.split_whitespace().collect();
        let Some(action) = words.first().copied() else {
            manager.pending().await;
            return self.notify(&manager.format_status(), NotifyLevel::Info);
        };
        if words.len() > 2 {
            return self.notify(MCP_USAGE, NotifyLevel::Warning);
        }
        manager.pending().await;
        let name = words.get(1).copied();
        match action {
            "login" | "logout" => {
                let none = "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.";
                let Some(server) = self.pick_mcp_server(
                    name,
                    none,
                    |status| status.oauth,
                    |status| status.state == Some(ServerState::NeedsAuth),
                ) else {
                    return;
                };
                if action == "logout" {
                    let removed = self.mcp_sign_out(&server).await;
                    let message = if removed {
                        format!("Signed out of MCP server \"{server}\".")
                    } else {
                        format!("No stored credentials for MCP server \"{server}\".")
                    };
                    return self.notify(&message, NotifyLevel::Info);
                }
                self.mcp_login(&server).await;
            }
            "reconnect" => {
                let none = "No enabled MCP server to reconnect.";
                let preferred = |status: &McpServerStatus| {
                    matches!(status.state, Some(ServerState::Failed | ServerState::Disconnected))
                };
                let Some(server) = self.pick_mcp_server(name, none, |status| status.state.is_some(), preferred) else {
                    return;
                };
                match self.mcp_reconnect(&server).await {
                    Some(failure) => self.notify(&failure, NotifyLevel::Error),
                    None => {
                        let state =
                            manager.status(&server).map(|status| describe_state(&status, true)).unwrap_or_default();
                        self.notify(&format!("Reconnected to MCP server \"{server}\" ({state})."), NotifyLevel::Info);
                    }
                }
            }
            _ => self.notify(MCP_USAGE, NotifyLevel::Warning),
        }
    }

    async fn mcp_login(&self, server: &str) {
        let prompt = lock(&self.builtins).sign_in_prompt.clone();
        let Some(prompt) = prompt else {
            let message = format!("Signing in to MCP server \"{server}\" requires interactive mode.");
            return self.notify(&message, NotifyLevel::Error);
        };
        match self.mcp_sign_in(server, prompt, None).await {
            Some(failure) => {
                let level = if failure == "Sign-in cancelled." { NotifyLevel::Info } else { NotifyLevel::Error };
                self.notify(&failure, level);
            }
            None => {
                let tools = self.mcp_manager().and_then(|manager| manager.status(server)).map_or(0, |s| s.tools.len());
                self.notify(&format!("Signed in to MCP server \"{server}\" ({tools} tools)."), NotifyLevel::Info);
            }
        }
    }

    /// The server a subcommand names, or the only (or only preferred) eligible one.
    fn pick_mcp_server(
        &self,
        name: Option<&str>,
        none: &str,
        eligible: impl Fn(&McpServerStatus) -> bool,
        preferred: impl Fn(&McpServerStatus) -> bool,
    ) -> Option<String> {
        let statuses: Vec<McpServerStatus> = self.mcp_statuses().into_iter().filter(|status| status.enabled).collect();
        if let Some(name) = name {
            let found = statuses.iter().find(|status| status.entry.name == name);
            match found {
                None => self.notify(&format!("No MCP server named \"{name}\"."), NotifyLevel::Error),
                Some(status) if !eligible(status) => self.notify(none, NotifyLevel::Error),
                Some(_) => return Some(name.to_string()),
            }
            return None;
        }
        let candidates: Vec<&McpServerStatus> = statuses.iter().filter(|status| eligible(status)).collect();
        let favored: Vec<&&McpServerStatus> = candidates.iter().filter(|status| preferred(status)).collect();
        match (candidates.as_slice(), favored.as_slice()) {
            ([], _) => self.notify(none, NotifyLevel::Info),
            ([only], _) => return Some(only.entry.name.clone()),
            (_, [only]) => return Some(only.entry.name.clone()),
            _ => {
                let names: Vec<&str> = candidates.iter().map(|status| status.entry.name.as_str()).collect();
                self.notify(&format!("Name the MCP server: {}", names.join(", ")), NotifyLevel::Warning);
            }
        }
        None
    }
}
