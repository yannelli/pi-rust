//! The built-in MCP support of a session (upstream `extensions/mcp/index.ts`): servers from
//! `mcp.json` (the project file only when the project is trusted) connect in the background, their
//! tools register with the session, and codemode or `tool_search` is activated to reach them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use pi_agent::{SharedTool, ToolExposure};
use pi_mcp::manager::UpdateConfigFn;
use pi_mcp::oauth::CallbackServerFactory;
use pi_mcp::runtime::{McpOAuthCredentialStore, TransportFactory};
use pi_mcp::{LoadedMcpConfig, MCP_SERVERS_SECTION, McpManager, McpManagerOptions, McpServerRegistry, load_mcp_config};

use super::same_tool;
use crate::agent_session::tools::ToolSource;
use crate::agent_session::{AgentSession, lock};
use crate::system_prompt::BuildSystemPromptOptions;

/// Loads the MCP config for a session cwd, in place of reading `mcp.json`.
pub type LoadMcpConfigFn = Arc<dyn Fn(&Path) -> LoadedMcpConfig + Send + Sync>;

/// How sessions run MCP (upstream `McpExtensionOptions`). Unset fields use the defaults.
#[derive(Clone, Default)]
pub struct McpSessionOptions {
    /// Default: `mcp.json` in the agent directory and, in trusted projects, `.pi/mcp.json`.
    pub load_config: Option<LoadMcpConfigFn>,
    /// Default: stdio and streamable HTTP transports built from the server config.
    pub create_transport: Option<TransportFactory>,
    /// Default: `mcp-auth.json` in the agent directory.
    pub credentials: Option<McpOAuthCredentialStore>,
    /// Default: `mcp.log` in the agent directory.
    pub log_path: Option<PathBuf>,
    /// How long the first prompt waits for servers with `direct` tools. Default: 10 s.
    pub startup_wait: Option<Duration>,
    /// Receives OAuth redirects during sign-in. Default: the loopback callback server.
    pub callback_factory: Option<Arc<dyn CallbackServerFactory>>,
    /// Saves `/mcp` changes. Default: edit the `mcp.json` that defines the server.
    pub update_config: Option<UpdateConfigFn>,
    /// Servers extensions register with `pi.registerMcpServer()`; they connect while registered.
    pub registry: Option<McpServerRegistry>,
}

fn mcp_source() -> ToolSource {
    ToolSource::builtin("mcp")
}

/// Set or remove the `mcp_servers` section of prompt options.
pub(crate) fn set_servers_section(options: &mut BuildSystemPromptOptions, section: Option<String>) {
    options.sections.retain(|(name, _)| name != MCP_SERVERS_SECTION);
    if let Some(section) = section {
        options.sections.push((MCP_SERVERS_SECTION.to_string(), section));
    }
}

impl AgentSession {
    /// The session's MCP manager; `None` when built-in MCP support is off or shut down.
    pub fn mcp_manager(&self) -> Option<McpManager> {
        lock(&self.builtins).mcp.clone()
    }

    fn create_mcp_manager(&self) -> McpManager {
        let configured = lock(&self.builtins).mcp_options.clone();
        let mut options = McpManagerOptions::new(&self.services.agent_dir);
        if let Some(create_transport) = configured.create_transport {
            options.create_transport = create_transport;
        }
        options.credentials = configured.credentials;
        options.log_path = configured.log_path;
        if let Some(wait) = configured.startup_wait {
            options.startup_wait = wait;
        }
        if let Some(factory) = configured.callback_factory {
            options.callback_factory = factory;
        }
        options.update_config = configured.update_config;
        options.registry = configured.registry;
        let weak = self.weak.clone();
        options.on_registered_change = Some(Arc::new(move || {
            if let Some(session) = weak.upgrade() {
                session.ensure_mcp_discovery();
            }
        }));
        let runtime = self.model_runtime().clone();
        options.provider_token = Some(Arc::new(move |provider: String| {
            let runtime = runtime.clone();
            async move { runtime.api_key_for(&provider).await }.boxed()
        }));
        let weak = self.weak.clone();
        options.notify = Some(Arc::new(move |message: &str, level| {
            if let Some(session) = weak.upgrade() {
                session.notify(message, level);
            }
        }));
        let manager = McpManager::new(options);
        let weak = self.weak.clone();
        manager.subscribe(move || {
            if let Some(session) = weak.upgrade() {
                session.sync_mcp_tools();
            }
        });
        manager
    }

    /// Start MCP for the session when `builtin:mcp` is enabled: load the config, connect the
    /// servers in the background, and activate codemode or `tool_search` from the config.
    pub(crate) fn start_mcp(&self) {
        if !self.builtin_extension_enabled("mcp") || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let manager = self.create_mcp_manager();
        let load_config = {
            let mut builtins = lock(&self.builtins);
            builtins.mcp = Some(manager.clone());
            builtins.mcp_options.load_config.clone()
        };
        let cwd = self.cwd().to_path_buf();
        let loaded = match load_config {
            Some(load) => load(&cwd),
            None => load_mcp_config(&self.services.agent_dir, &cwd, self.settings().is_project_trusted()),
        };
        manager.start(cwd, loaded);
        self.ensure_mcp_discovery();
    }

    /// Stop MCP and drop its tools; `shutdown` closes the connections in the background.
    pub(crate) fn stop_mcp(&self) -> Option<McpManager> {
        let manager = {
            let mut builtins = lock(&self.builtins);
            builtins.mcp_tools.clear();
            builtins.mcp.take()
        };
        self.replace_source_tools(Vec::new(), &mcp_source());
        manager
    }

    /// Register the manager's tools with the session. Tools that stop being `direct` leave the
    /// active set, as upstream `setExposure` and the resource tools do.
    pub(crate) fn sync_mcp_tools(&self) {
        let mut builtins = lock(&self.builtins);
        let Some(manager) = builtins.mcp.clone() else { return };
        let tools: Vec<SharedTool> = manager.tools().iter().map(|tool| tool.agent_tool()).collect();
        let previous = std::mem::take(&mut builtins.mcp_tools);
        let unchanged = previous.len() == tools.len() && previous.iter().zip(&tools).all(|(a, b)| same_tool(a, b));
        builtins.mcp_tools = tools.clone();
        if unchanged {
            return;
        }
        let was_direct =
            |name: &str| previous.iter().any(|tool| tool.name() == name && tool.exposure() == ToolExposure::Direct);
        let demoted: Vec<String> = tools
            .iter()
            .filter(|tool| matches!(tool.exposure(), ToolExposure::Codemode | ToolExposure::Deferred))
            .filter(|tool| was_direct(tool.name()))
            .map(|tool| tool.name().to_string())
            .collect();
        {
            let mut state = lock(&self.tools);
            state.replace_source(&mcp_source().path, tools, &mcp_source());
            state.refresh(Default::default());
            if !demoted.is_empty() {
                let active: Vec<String> =
                    state.registry.active_names().into_iter().filter(|name| !demoted.contains(name)).collect();
                if state.set_active(&active) {
                    state.pending.clear();
                }
            }
        }
        drop(builtins);
        self.sync_agent_tools();
    }

    /// Activate codemode or `tool_search` when MCP tools are only reachable through them (upstream
    /// `ensureDiscoveryActive`), and warn once when neither can be. Only a tool of these names that
    /// declares `reaches_tools` can reach MCP tools; another extension's tool of the same name
    /// without it is never activated.
    pub(crate) fn ensure_mcp_discovery(&self) {
        let Some(manager) = lock(&self.builtins).mcp.clone() else { return };
        let (has_codemode, has_tool_search, active) = {
            let tools = lock(&self.tools);
            let reaches = |name: &str| {
                let tool = tools.registry.get(name);
                tool.is_some_and(|tool| tool.reaches_tools(&serde_json::json!({})).is_some())
            };
            let has_codemode = reaches(pi_codemode::CODEMODE_TOOL_NAME);
            (has_codemode, reaches(pi_codemode::TOOL_SEARCH_TOOL_NAME), tools.registry.active_names())
        };
        let activate = manager.ensure_discovery_active(has_codemode, has_tool_search, &active);
        if !activate.is_empty() {
            let mut next = active;
            next.extend(activate);
            self.set_active_tools_by_name(&next);
        }
    }

    /// Before a prompt: pick up sign-ins made elsewhere, wait once for servers with `direct`
    /// tools, and return the `mcp_servers` section as the servers are now.
    pub(crate) async fn prepare_mcp_for_prompt(&self) -> Option<String> {
        let manager = self.mcp_manager()?;
        manager.reconnect_signed_in().await;
        manager.wait_for_direct_servers().await;
        self.ensure_mcp_discovery();
        manager.servers_section()
    }

    /// At the start of a later turn: reconnect servers whose credentials were stored meanwhile.
    pub(crate) async fn mcp_turn_start(&self) {
        let Some(manager) = self.mcp_manager() else { return };
        manager.reconnect_signed_in().await;
        self.ensure_mcp_discovery();
    }

    /// Close the MCP connections and wait for running sign-ins (upstream `session_shutdown`).
    pub async fn shutdown(&self) {
        if let Some(manager) = self.stop_mcp() {
            manager.shutdown().await;
        }
    }
}
