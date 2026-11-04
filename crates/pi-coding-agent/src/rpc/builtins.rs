//! The built-in extensions in RPC mode (upstream `extensions/mcp` with `ctx.mode === "rpc"`):
//! notices become `notify` requests, `/mcp login` asks for the redirect URL through an `input`
//! dialog, and `get_commands` lists `/mcp` and the built-in commands as extension commands.

use std::sync::Arc;

use async_trait::async_trait;
use pi_mcp::NotifyLevel;
use pi_mcp::runtime::McpSignInPrompt;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::types::{RpcCommandSource, RpcSlashCommand};
use super::ui::RpcExtensionUi;
use crate::agent_session::AgentSession;
use crate::extension_ui::{DialogOptions, ExtensionUi, NotifyType};
use crate::headless_commands::{HEADLESS_COMMANDS, SESSION_COMMANDS};
use crate::resources::source_info::{BUILTIN_PATH_PREFIX, create_synthetic_source_info};

/// Upstream's `/mcp` command description.
const MCP_DESCRIPTION: &str = "Manage MCP servers: sign in, reconnect, enable or disable, and change exposure";

fn notify_type(level: NotifyLevel) -> NotifyType {
    match level {
        NotifyLevel::Info => NotifyType::Info,
        NotifyLevel::Warning => NotifyType::Warning,
        NotifyLevel::Error => NotifyType::Error,
    }
}

/// MCP OAuth sign-in over the extension UI.
struct RpcMcpSignIn {
    ui: Arc<RpcExtensionUi>,
}

#[async_trait]
impl McpSignInPrompt for RpcMcpSignIn {
    fn show_authorization_url(&self, server: &str, url: &Url) {
        let message = format!("Sign in to MCP server \"{server}\" in your browser:\n{url}");
        self.ui.notify(&message, Some(NotifyType::Info));
        pi_ai::auth::oauth::open_browser(url.as_str());
    }

    async fn prompt_for_redirect_url(&self, server: &str, cancel: CancellationToken) -> Option<String> {
        let title = format!(
            "Waiting for sign-in to \"{server}\". If the browser cannot reach this machine, paste the URL it was redirected to."
        );
        let options = DialogOptions { cancel: Some(cancel), ..DialogOptions::default() };
        self.ui.input(&title, Some("http://127.0.0.1:.../callback?code=..."), options).await
    }
}

/// Route the built-in extensions' notices and sign-in prompts of `session` to the client.
pub(crate) fn bind_builtins(session: &AgentSession, ui: &Arc<RpcExtensionUi>) {
    let notify_ui = ui.clone();
    session.set_notify_handler(Some(Arc::new(move |message: &str, level: NotifyLevel| {
        notify_ui.notify(message, Some(notify_type(level)));
    })));
    session.set_mcp_sign_in_prompt(Some(Arc::new(RpcMcpSignIn { ui: ui.clone() })));
}

/// Detach `session` from the client once RPC mode stops serving it.
pub(crate) fn unbind_builtins(session: &AgentSession) {
    session.set_notify_handler(None);
    session.set_mcp_sign_in_prompt(None);
}

/// Built-in commands: `/mcp` while MCP runs for the session, then the commands RPC mode runs
/// without a model call. They are listed like `/mcp`, as extension commands with a
/// `builtin:<name>` path, so upstream clients parse them.
pub fn builtin_commands(session: &AgentSession) -> Vec<RpcSlashCommand> {
    let mut commands: Vec<RpcSlashCommand> = mcp_command(session).into_iter().collect();
    commands.extend(HEADLESS_COMMANDS.iter().chain(&SESSION_COMMANDS).map(|command| {
        let path = format!("{BUILTIN_PATH_PREFIX}{}", command.name);
        RpcSlashCommand {
            name: command.name.into(),
            description: Some(command.description.into()),
            source: RpcCommandSource::Extension,
            source_info: create_synthetic_source_info(&path, "builtin", None, None),
        }
    }));
    commands
}

fn mcp_command(session: &AgentSession) -> Option<RpcSlashCommand> {
    session.mcp_manager()?;
    let path = format!("{BUILTIN_PATH_PREFIX}mcp");
    let services = session.services();
    let loader = services.resources();
    let entry = loader.resources().extensions.iter().find(|entry| entry.path.to_string_lossy() == path)?;
    Some(RpcSlashCommand {
        name: "mcp".into(),
        description: Some(MCP_DESCRIPTION.into()),
        source: RpcCommandSource::Extension,
        source_info: entry.source_info.clone(),
    })
}
