//! MCP in the TUI: session notices, the browser sign-in prompt, `/mcp`, the servers listed at
//! startup, and the Radius MCP offer after a Radius login (upstream `extensions/mcp` command
//! handlers and `offerRadiusMcpServer`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use pi_ai::auth::oauth::{DEFAULT_RADIUS_GATEWAY, normalize_radius_gateway_url};
use pi_mcp::config::{ProviderAuth, add_mcp_server_config};
use pi_mcp::manager::describe_state;
use pi_mcp::runtime::{McpSignInPrompt, ServerState};
use pi_mcp::{McpServerConfig, McpServerStatus, NotifyLevel, load_mcp_config};
use pi_tui::AutocompleteItem;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::bridge::{EventBridge, UiEvent};
use super::mode::InteractiveMode;
use crate::interactive_components::login::system_browser;
use crate::interactive_components::{McpManagerView, McpServers, run_mcp_manager, sign_in_with_view};
use crate::theme::{ThemeColor, theme};

const MCP_ACTIONS: [&str; 3] = ["login", "logout", "reconnect"];
const NO_OAUTH_SERVER: &str = "No enabled MCP server uses OAuth. Only HTTP servers without an Authorization header do.";

/// `/mcp login` through the UI thread: a notice with the URL, and an input for the redirect URL.
struct UiSignInPrompt {
    bridge: Weak<EventBridge>,
}

#[async_trait]
impl McpSignInPrompt for UiSignInPrompt {
    fn show_authorization_url(&self, server: &str, url: &Url) {
        let Some(bridge) = self.bridge.upgrade() else { return };
        let message = format!("Sign in to MCP server \"{server}\" in your browser:\n{url}");
        bridge.push(UiEvent::Notify { message, level: NotifyLevel::Info });
        pi_ai::auth::oauth::open_browser(url.as_str());
    }

    async fn prompt_for_redirect_url(&self, server: &str, cancel: CancellationToken) -> Option<String> {
        let bridge = self.bridge.upgrade()?;
        let (reply, answer) = oneshot::channel();
        let title = format!(
            "Waiting for sign-in to \"{server}\". If the browser cannot reach this machine, paste the URL it was redirected to."
        );
        bridge.call(move |mode| {
            let receiver = mode.ask_input_until(&title, None, Some(cancel));
            tokio::task::spawn_local(async move {
                let _ = reply.send(receiver.await.ok().flatten());
            });
        });
        answer.await.ok().flatten()
    }
}

fn radius_mcp_url() -> String {
    format!("{}/mcp", normalize_radius_gateway_url(DEFAULT_RADIUS_GATEWAY))
}

impl InteractiveMode {
    /// Upstream `showExtensionNotify`.
    pub(super) fn show_notice(&self, message: &str, level: NotifyLevel) {
        match level {
            NotifyLevel::Error => self.show_error(message),
            NotifyLevel::Warning => self.show_warning(message),
            NotifyLevel::Info => self.show_status(message),
        }
    }

    /// Route the session's notices and MCP sign-in prompts to this UI.
    pub(super) fn install_session_hooks(&self) {
        let session = self.session();
        let bridge = Arc::downgrade(&self.bridge);
        let notices = bridge.clone();
        session.set_notify_handler(Some(Arc::new(move |message: &str, level| {
            if let Some(bridge) = notices.upgrade() {
                bridge.push(UiEvent::Notify { message: message.to_string(), level });
            }
        })));
        session.set_mcp_sign_in_prompt(Some(Arc::new(UiSignInPrompt { bridge })));
    }

    /// `/mcp [login|logout|reconnect] [server]`: the manager view without arguments, the sign-in
    /// view for `login`, and the session's handler for the rest.
    pub(super) fn handle_mcp_command(&self, args: Option<String>) {
        let session = self.session();
        let args = args.unwrap_or_default();
        let words: Vec<String> = args.split_whitespace().map(String::from).collect();
        self.spawn(move |mode| async move {
            match words.as_slice() {
                [] => mode.run_mcp_view().await,
                [action, rest @ ..] if action == "login" && rest.len() <= 1 => {
                    mode.mcp_login(rest.first().cloned()).await
                }
                _ => {
                    let _ = tokio::spawn(async move { session.run_mcp_command(&args).await }).await;
                    mode.request_render();
                }
            }
        });
    }

    async fn run_mcp_view(self: &Rc<Self>) {
        let view = Rc::new(RefCell::new(McpManagerView::new(&self.tui())));
        let close = self.mount_view(view.clone());
        let servers: Rc<dyn McpServers> = Rc::new(self.session());
        run_mcp_manager(&view, servers, system_browser()).await;
        close();
    }

    /// The server `/mcp login` names, or the only (or only waiting) OAuth server, else a choice.
    async fn pick_oauth_server(&self, name: Option<String>) -> Option<String> {
        let candidates: Vec<McpServerStatus> =
            self.session().mcp_statuses().into_iter().filter(|status| status.enabled && status.oauth).collect();
        if let Some(name) = name {
            if candidates.iter().any(|status| status.entry.name == name) {
                return Some(name);
            }
            let known = self.session().mcp_statuses().iter().any(|status| status.entry.name == name);
            self.show_error(&if known {
                NO_OAUTH_SERVER.to_string()
            } else {
                format!("No MCP server named \"{name}\".")
            });
            return None;
        }
        let waiting: Vec<&McpServerStatus> =
            candidates.iter().filter(|status| status.state == Some(ServerState::NeedsAuth)).collect();
        match (candidates.as_slice(), waiting.as_slice()) {
            ([], _) => {
                self.show_status(NO_OAUTH_SERVER);
                None
            }
            ([only], _) => Some(only.entry.name.clone()),
            (_, [only]) => Some(only.entry.name.clone()),
            _ => {
                let names = candidates.iter().map(|status| status.entry.name.clone()).collect();
                self.ask_choice("MCP server", names).await.ok().flatten()
            }
        }
    }

    /// `/mcp login [server]` with the sign-in view.
    async fn mcp_login(self: &Rc<Self>, name: Option<String>) {
        let waiting = self.session();
        let _ = tokio::spawn(async move { waiting.mcp_pending().await }).await;
        let Some(name) = self.pick_oauth_server(name).await else { return };
        let view = Rc::new(RefCell::new(McpManagerView::new(&self.tui())));
        let close = self.mount_view(view.clone());
        let servers: Rc<dyn McpServers> = Rc::new(self.session());
        let failure = sign_in_with_view(&view, &servers, &name, &system_browser()).await;
        close();
        match failure {
            Some(failure) if failure == "Sign-in cancelled." => self.show_status(&failure),
            Some(failure) => self.show_error(&failure),
            None => {
                let tools = servers.status(&name).map_or(0, |status| status.tools.len());
                self.show_status(&format!("Signed in to MCP server \"{name}\" ({tools} tools)."));
            }
        }
    }

    /// Argument completion of `/mcp`: the actions, then the servers each action applies to.
    pub(super) fn mcp_completions(&self, prefix: &str) -> Option<Vec<AutocompleteItem>> {
        let words: Vec<&str> = prefix.split_whitespace().collect();
        let trailing_space = prefix.ends_with(char::is_whitespace);
        let (action, server) = match (words.as_slice(), trailing_space) {
            ([], _) => ("", None),
            ([action], false) => (*action, None),
            ([action], true) => (*action, Some("")),
            ([action, server], false) => (*action, Some(*server)),
            _ => return None,
        };
        let Some(server) = server else {
            let items: Vec<AutocompleteItem> = MCP_ACTIONS
                .iter()
                .filter(|item| item.starts_with(action))
                .map(|item| AutocompleteItem::new(format!("{item} "), *item))
                .collect();
            return (!items.is_empty()).then_some(items);
        };
        if !MCP_ACTIONS.contains(&action) {
            return None;
        }
        let items: Vec<AutocompleteItem> = self
            .session()
            .mcp_statuses()
            .into_iter()
            .filter(|status| if action == "reconnect" { status.state.is_some() } else { status.oauth })
            .filter(|status| status.entry.name.starts_with(server))
            .map(|status| {
                let name = status.entry.name.clone();
                AutocompleteItem::new(format!("{action} {name}"), name).with_description(describe_state(&status, false))
            })
            .collect();
        (!items.is_empty()).then_some(items)
    }

    /// The `[MCP]` section of the startup header: configured servers and their state.
    pub(super) fn add_mcp_servers_section(&self) {
        let statuses = self.session().mcp_statuses();
        if statuses.is_empty() {
            return;
        }
        let names: Vec<String> = statuses.iter().map(|status| status.entry.name.clone()).collect();
        let details: Vec<String> = statuses
            .iter()
            .map(|status| {
                let state = theme().fg(ThemeColor::Dim, &describe_state(status, false));
                format!("  {} {state}\n    {}", status.entry.name, theme().fg(ThemeColor::Dim, &status.entry.source))
            })
            .collect();
        self.add_loaded_section(
            "MCP",
            move || super::resources_view::compact_list(names.clone(), true),
            move || details.join("\n"),
        );
    }

    /// After a Radius login, offer to point the global Radius MCP server at that login.
    pub(super) fn offer_radius_mcp_server(&self, provider_id: &str, provider_name: &str) {
        let mcp_path = self.agent_dir.join("mcp.json");
        let normalize = |url: &str| url.trim_end_matches('/').to_string();
        let target = normalize(&radius_mcp_url());
        let loaded = load_mcp_config(&self.agent_dir, self.session().cwd(), false);
        let existing = loaded
            .servers
            .iter()
            .find(|server| server.config.url.as_deref().is_some_and(|url| normalize(url) == target))
            .cloned();
        if existing
            .as_ref()
            .and_then(|server| server.config.auth.as_ref())
            .is_some_and(|auth| auth.provider == provider_id)
        {
            return;
        }
        let mut name = existing.as_ref().map_or_else(|| "radius".to_string(), |server| server.name.clone());
        if existing.is_none() && loaded.servers.iter().any(|server| server.name == name) {
            name = "radius-mcp".to_string();
        }
        let mut config = existing.map_or_else(|| McpServerConfig::http(radius_mcp_url()), |server| server.config);
        config.auth = Some(ProviderAuth { provider: provider_id.to_string() });
        config.oauth = None;
        let title = format!("Configure {provider_name} MCP in {}?", mcp_path.display());
        let answer = self.ask_choice(&title, vec!["Yes".to_string(), "No".to_string()]);
        self.spawn(move |mode| async move {
            if answer.await.ok().flatten().as_deref() != Some("Yes") {
                return mode.request_render();
            }
            let value = serde_json::to_value(&config).unwrap_or_default();
            if let Err(error) = add_mcp_server_config(&mcp_path, &name, &value) {
                return mode.show_error(&format!("Could not update {}: {error}", mcp_path.display()));
            }
            mode.handle_reload_command();
        });
    }
}
