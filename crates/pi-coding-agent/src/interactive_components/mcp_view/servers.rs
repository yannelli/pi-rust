//! What the `/mcp` view needs of the MCP servers ([`McpServers`], implemented by
//! [`pi_mcp::McpManager`] and the session), and the menus built from them (upstream
//! `serversMenu`, `serverMenu`, `showTools`, and `chooseExposure` in `extensions/mcp/index.ts`).

use std::sync::Arc;

use async_trait::async_trait;
use pi_mcp::config::get_mcp_tool_exposure;
use pi_mcp::manager::describe_state;
use pi_mcp::runtime::{McpSignInPrompt, ServerState};
use pi_mcp::{McpConfigScope, McpConnectionId, McpExposure, McpManager, McpServerStatus, McpSubscription, Tool};
use pi_tui::components::SelectItem;
use tokio_util::sync::CancellationToken;

use super::McpMenu;
use crate::agent_session::AgentSession;

/// What each exposure means, in menu order.
pub const EXPOSURE_DESCRIPTIONS: [(McpExposure, &str); 3] = [
    (McpExposure::Codemode, "called from codemode scripts, which find them with searchTools()"),
    (McpExposure::Deferred, "not declared until tool_search loads them, then called directly; no codemode needed"),
    (McpExposure::Direct, "declared to the model like built-in tools"),
];

/// The server state and actions the `/mcp` view uses. Each action returns the message to show
/// when it failed.
#[async_trait(?Send)]
pub trait McpServers {
    /// Every server, the ones that need the user first, then by name.
    fn statuses(&self) -> Vec<McpServerStatus>;
    fn status(&self, name: &str) -> Option<McpServerStatus> {
        self.statuses().into_iter().find(|status| status.entry.name == name)
    }
    fn config_errors(&self) -> Vec<String>;
    /// Shown above the servers: config errors and registered servers `mcp.json` overrides.
    fn notices(&self) -> Vec<String> {
        self.config_errors().iter().map(|error| format!("config: {error}")).collect()
    }
    /// The tools a connected server offers.
    fn tools(&self, name: &str) -> Vec<Tool>;
    /// Whether a global server can be turned on or off for the trusted project alone.
    fn can_save_in_project(&self, name: &str) -> bool;
    /// Shown when no servers are configured.
    fn empty_message(&self) -> String;
    /// Called whenever servers, their states, or their tools change, until `unsubscribe`.
    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Option<McpSubscription>;
    fn unsubscribe(&self, _subscription: McpSubscription) {}
    /// The server's current connection try, which changes when it is disabled or replaced.
    fn connection_id(&self, _name: &str) -> Option<McpConnectionId> {
        None
    }
    /// Shows the result of an action started for connection `id`, unless it is stale now.
    fn set_message_if_current(&self, name: &str, id: Option<McpConnectionId>, message: Option<String>) {
        if self.connection_id(name) == id {
            self.set_message(name, message);
        }
    }
    async fn sign_in(&self, name: &str, prompt: Arc<dyn McpSignInPrompt>, cancel: CancellationToken) -> Option<String>;
    /// Whether stored credentials were deleted.
    async fn sign_out(&self, name: &str) -> bool;
    async fn reconnect(&self, name: &str) -> Option<String>;
    async fn set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String>;
    fn set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String>;
    /// Shows `message` with the server, or clears it.
    fn set_message(&self, name: &str, message: Option<String>);
}

#[async_trait(?Send)]
impl McpServers for McpManager {
    fn statuses(&self) -> Vec<McpServerStatus> {
        self.statuses_by_attention()
    }

    fn status(&self, name: &str) -> Option<McpServerStatus> {
        McpManager::status(self, name)
    }

    fn config_errors(&self) -> Vec<String> {
        McpManager::config_errors(self)
    }

    fn notices(&self) -> Vec<String> {
        McpManager::notices(self)
    }

    fn tools(&self, name: &str) -> Vec<Tool> {
        self.connection(name).map(|connection| connection.tools()).unwrap_or_default()
    }

    fn can_save_in_project(&self, name: &str) -> bool {
        let Some(status) = McpManager::status(self, name) else { return false };
        status.entry.scope == Some(McpConfigScope::Global)
            && status.entry.override_path.is_none()
            && self.project_config().is_some()
    }

    fn empty_message(&self) -> String {
        self.no_servers_message()
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Option<McpSubscription> {
        Some(McpManager::subscribe(self, listener))
    }

    fn unsubscribe(&self, subscription: McpSubscription) {
        McpManager::unsubscribe(self, subscription);
    }

    fn connection_id(&self, name: &str) -> Option<McpConnectionId> {
        McpManager::connection_id(self, name)
    }

    fn set_message_if_current(&self, name: &str, id: Option<McpConnectionId>, message: Option<String>) {
        if let Some(id) = id {
            McpManager::set_message_if_current(self, name, id, message);
        }
    }

    async fn sign_in(&self, name: &str, prompt: Arc<dyn McpSignInPrompt>, cancel: CancellationToken) -> Option<String> {
        McpManager::sign_in(self, name, prompt, Some(cancel)).await
    }

    async fn sign_out(&self, name: &str) -> bool {
        McpManager::sign_out(self, name).await
    }

    async fn reconnect(&self, name: &str) -> Option<String> {
        McpManager::reconnect(self, name).await
    }

    async fn set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String> {
        McpManager::set_enabled(self, name, enabled, in_project).await
    }

    fn set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String> {
        McpManager::set_exposure(self, name, exposure)
    }

    fn set_message(&self, name: &str, message: Option<String>) {
        McpManager::set_message(self, name, message);
    }
}

/// The session's servers; actions also activate codemode or `tool_search` as needed.
#[async_trait(?Send)]
impl McpServers for Arc<AgentSession> {
    fn statuses(&self) -> Vec<McpServerStatus> {
        self.mcp_statuses()
    }

    fn config_errors(&self) -> Vec<String> {
        self.mcp_config_errors()
    }

    fn notices(&self) -> Vec<String> {
        self.mcp_manager().map(|manager| manager.notices()).unwrap_or_default()
    }

    fn tools(&self, name: &str) -> Vec<Tool> {
        self.mcp_manager().map(|manager| McpServers::tools(&manager, name)).unwrap_or_default()
    }

    fn can_save_in_project(&self, name: &str) -> bool {
        self.mcp_can_save_in_project(name)
    }

    fn empty_message(&self) -> String {
        self.mcp_manager().map_or_else(|| self.mcp_status_text(), |manager| manager.no_servers_message())
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Option<McpSubscription> {
        self.subscribe_mcp(listener)
    }

    fn unsubscribe(&self, subscription: McpSubscription) {
        self.unsubscribe_mcp(subscription);
    }

    fn connection_id(&self, name: &str) -> Option<McpConnectionId> {
        self.mcp_manager().and_then(|manager| manager.connection_id(name))
    }

    fn set_message_if_current(&self, name: &str, id: Option<McpConnectionId>, message: Option<String>) {
        if let (Some(manager), Some(id)) = (self.mcp_manager(), id) {
            manager.set_message_if_current(name, id, message);
        }
    }

    async fn sign_in(&self, name: &str, prompt: Arc<dyn McpSignInPrompt>, cancel: CancellationToken) -> Option<String> {
        self.mcp_sign_in(name, prompt, Some(cancel)).await
    }

    async fn sign_out(&self, name: &str) -> bool {
        self.mcp_sign_out(name).await
    }

    async fn reconnect(&self, name: &str) -> Option<String> {
        self.mcp_reconnect(name).await
    }

    async fn set_enabled(&self, name: &str, enabled: bool, in_project: bool) -> Option<String> {
        self.mcp_set_enabled(name, enabled, in_project).await
    }

    fn set_exposure(&self, name: &str, exposure: McpExposure) -> Option<String> {
        self.mcp_set_exposure(name, exposure)
    }

    fn set_message(&self, name: &str, message: Option<String>) {
        if let Some(manager) = self.mcp_manager() {
            manager.set_message(name, message);
        }
    }
}

fn item(value: &str, label: &str, description: Option<&str>) -> SelectItem {
    let item = SelectItem::new(value, label);
    match description {
        Some(description) => item.with_description(description),
        None => item,
    }
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or_default()
}

fn exposure_description(exposure: McpExposure) -> &'static str {
    let described = EXPOSURE_DESCRIPTIONS.iter().find(|(candidate, _)| *candidate == exposure);
    described.map_or("unreachable", |(_, description)| description)
}

fn scope_name(status: &McpServerStatus) -> Option<String> {
    status.entry.scope.map(|scope| scope.to_string())
}

/// The servers, the ones that need the user first.
pub fn servers_menu(servers: &dyn McpServers) -> McpMenu {
    let errors = servers.notices();
    let statuses = servers.statuses();
    let items = statuses.iter().map(|status| {
        let place = match &status.entry.override_path {
            Some(_) => "global, project override".to_string(),
            None => scope_name(status).unwrap_or_else(|| status.entry.source.clone()),
        };
        let exposure = status.entry.config.exposure().as_str();
        let description = format!("{} · {exposure} · {place}", describe_state(status, true));
        item(&status.entry.name, &status.entry.name, Some(&description))
    });
    McpMenu {
        title: "MCP servers".into(),
        error: (!errors.is_empty()).then(|| errors.join("\n")),
        items: items.collect(),
        empty: Some(servers.empty_message()),
        confirm_label: "manage".into(),
        cancel_label: "close".into(),
        ..McpMenu::default()
    }
}

/// The actions for one server, with its transport, config, and state.
pub fn server_menu(servers: &dyn McpServers, name: &str) -> McpMenu {
    let Some(status) = servers.status(name) else {
        return McpMenu {
            title: name.into(),
            empty: Some("This server is no longer configured.".into()),
            cancel_label: "back".into(),
            ..McpMenu::default()
        };
    };
    let entry = &status.entry;
    let saved = match (entry.scope, &entry.override_path) {
        (Some(McpConfigScope::Extension), _) => "for this session".to_string(),
        (Some(McpConfigScope::Client), _) => "for this ACP session".to_string(),
        (_, Some(_)) => "saved to the project mcp.json".into(),
        (Some(scope), None) => format!("saved to the {scope} mcp.json"),
        (None, None) => "saved to mcp.json".into(),
    };
    let in_project = servers.can_save_in_project(name);
    let project_saved = Some("saved to the project mcp.json");
    let mut items = Vec::new();
    if !status.enabled {
        items.push(item("enable", "Enable", Some(&saved)));
        if in_project {
            items.push(item("enable-project", "Enable in this project", project_saved));
        }
    } else {
        let state = status.state;
        if state == Some(ServerState::NeedsAuth) {
            items.push(item("signin", "Sign in", Some("opens the browser")));
        }
        if state == Some(ServerState::Connected) {
            items.push(item("tools", "Tools", Some(&format!("{} offered", status.tools.len()))));
        }
        let reconnectable =
            [ServerState::Failed, ServerState::Disconnected, ServerState::Connected, ServerState::NeedsAuth];
        if state.is_some_and(|state| reconnectable.contains(&state)) {
            items.push(item("reconnect", "Reconnect", None));
        }
        if state == Some(ServerState::Connected) && status.oauth {
            items.push(item("signout", "Sign out", Some("deletes the stored credentials")));
        }
        items.push(item("exposure", "Exposure", Some(entry.config.exposure().as_str())));
        items.push(item("disable", "Disable", Some(&saved)));
        if in_project {
            items.push(item("disable-project", "Disable in this project", project_saved));
        }
    }
    let transport = match &entry.config.url {
        Some(url) => url.clone(),
        None if entry.scope == Some(McpConfigScope::Client) => "stdio (command and args not shown)".to_string(),
        None => {
            let command = entry.config.command.iter().chain(entry.config.args.iter().flatten());
            command.map(String::as_str).collect::<Vec<_>>().join(" ")
        }
    };
    let mut details =
        vec![transport, format!("{}: {}", scope_name(&status).as_deref().unwrap_or("config"), entry.source)];
    if let Some(path) = &entry.override_path {
        details.push(format!("project override: {path}"));
    }
    details.push(format!("State: {}", describe_state(&status, false)));
    let connection_error = status.error.clone().filter(|_| status.state != Some(ServerState::Connected));
    let error: Vec<String> = status.message.clone().into_iter().chain(connection_error).collect();
    McpMenu {
        title: format!("MCP server {name}"),
        details: Some(details.join("\n")),
        error: (!error.is_empty()).then(|| error.join("\n")),
        selected: items.first().map(|item: &SelectItem| item.value.clone()),
        items,
        empty: None,
        confirm_label: "select".into(),
        cancel_label: "back".into(),
    }
}

/// The tools of a connected server, marking the ones with their own exposure.
pub fn tools_menu(status: &McpServerStatus, tools: &[Tool]) -> McpMenu {
    let config = &status.entry.config;
    let exposure = config.exposure();
    let overridden = config.tool_exposure.as_ref().is_some_and(|map| !map.is_empty());
    let note = if overridden { "\nSome tools override it with toolExposure." } else { "" };
    let items = tools.iter().map(|tool| {
        let tool_exposure = get_mcp_tool_exposure(config, &tool.name);
        let description = first_line(tool.description.as_deref().unwrap_or_default());
        let description = if tool_exposure == exposure {
            description.to_string()
        } else {
            format!("[{tool_exposure}] {description}")
        };
        item(&tool.name, &tool.name, Some(&description))
    });
    McpMenu {
        title: format!("Tools of {}", status.entry.name),
        details: Some(format!("Exposure {exposure}: {}{note}", exposure_description(exposure))),
        items: items.collect(),
        empty: Some("The server offers no tools.".into()),
        confirm_label: "back".into(),
        cancel_label: "back".into(),
        ..McpMenu::default()
    }
}

/// The exposures to choose from, with the current one checked and selected.
pub fn exposure_menu(status: &McpServerStatus) -> McpMenu {
    let entry = &status.entry;
    let current = entry.config.exposure();
    let details = match entry.scope {
        Some(McpConfigScope::Extension) => {
            format!("Applies to this session; the server is registered by {}.", entry.source)
        }
        Some(McpConfigScope::Client) => "Applies to this session; the ACP client supplied the server.".to_string(),
        _ => format!("Saved to {}.", entry.override_path.as_deref().unwrap_or(&entry.source)),
    };
    let items = EXPOSURE_DESCRIPTIONS.iter().map(|(exposure, description)| {
        let mark = if *exposure == current { "✓ " } else { "  " };
        item(exposure.as_str(), &format!("{mark}{exposure}"), Some(description))
    });
    McpMenu {
        title: format!("Exposure of {}", entry.name),
        details: Some(details),
        items: items.collect(),
        selected: Some(current.as_str().into()),
        confirm_label: "save".into(),
        cancel_label: "back".into(),
        ..McpMenu::default()
    }
}
