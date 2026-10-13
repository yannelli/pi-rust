//! Built-in extensions ported natively (upstream `src/extensions/*`): `builtin:codemode`,
//! `builtin:tool-search`, and `builtin:mcp`. Each loads unless `-builtin:<name>` in the
//! `extensions` setting, `--no-extensions`, or (for MCP) `--no-mcp` turns it off.

mod codemode;
mod codemode_host;
pub mod mcp;
mod mcp_api;
mod tool_search;

use std::sync::Arc;

use pi_agent::SharedTool;
use pi_mcp::McpManager;
use pi_mcp::runtime::McpSignInPrompt;
use serde_json::Value;

pub use self::codemode::{CODEMODE_DOCS, SessionCodemodeTool, codemode_docs_path};
pub use self::mcp::{LoadMcpConfigFn, McpSessionOptions};
pub use self::tool_search::ToolSearchTool;
pub use pi_mcp::NotifyLevel;

use super::tools::{ActiveSelection, ToolSource};
use super::{AgentSession, AgentSessionEvent, lock};
use crate::resources::source_info::BUILTIN_PATH_PREFIX;

/// Names of the built-in extensions, listed as `builtin:<name>` extension resources.
/// `llama.cpp` is toggled here too; its provider is wired elsewhere.
pub const BUILTIN_EXTENSIONS: [&str; 4] = ["llama.cpp", "codemode", "tool-search", "mcp"];

/// Shows a message to the user (upstream `ctx.ui.notify`).
pub type NotifyFn = Arc<dyn Fn(&str, NotifyLevel) + Send + Sync>;

/// Messages kept for a notify handler that is not set yet.
const MAX_QUEUED_NOTICES: usize = 32;

#[derive(Default)]
pub(crate) struct BuiltinState {
    /// The built-in `codemode` and `tool_search` tools, told apart from other tools of those names.
    pub codemode: Option<SharedTool>,
    pub tool_search: Option<SharedTool>,
    pub mcp: Option<McpManager>,
    pub mcp_options: McpSessionOptions,
    /// MCP tools as last registered with the session.
    pub mcp_tools: Vec<SharedTool>,
    pub sign_in_prompt: Option<Arc<dyn McpSignInPrompt>>,
    pub notify: Option<NotifyFn>,
    pub notices: Vec<(String, NotifyLevel)>,
}

/// Whether two handles name the same tool instance.
pub(crate) fn same_tool(a: &SharedTool, b: &SharedTool) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

impl AgentSession {
    /// Whether the built-in extension `name` (such as `mcp`) is enabled for this session.
    pub fn builtin_extension_enabled(&self, name: &str) -> bool {
        let path = format!("{BUILTIN_PATH_PREFIX}{name}");
        let services = self.services();
        let loader = services.resources();
        loader.resources().extensions.iter().any(|entry| entry.path.to_string_lossy() == path)
    }

    /// Where messages for the user go (upstream `ctx.ui.notify`). Messages sent before a handler
    /// is set are delivered to it; without one they are dropped, as in print mode.
    pub fn set_notify_handler(&self, handler: Option<NotifyFn>) {
        let queued = {
            let mut builtins = lock(&self.builtins);
            builtins.notify = handler.clone();
            if handler.is_some() { std::mem::take(&mut builtins.notices) } else { Vec::new() }
        };
        if let Some(handler) = handler {
            for (message, level) in queued {
                handler(&message, level);
            }
        }
    }

    pub(crate) fn notify(&self, message: &str, level: NotifyLevel) {
        let handler = {
            let mut builtins = lock(&self.builtins);
            if builtins.notify.is_none() && builtins.notices.len() < MAX_QUEUED_NOTICES {
                builtins.notices.push((message.to_string(), level));
            }
            builtins.notify.clone()
        };
        if let Some(handler) = handler {
            handler(message, level);
        }
    }

    /// Append a custom entry to the session (upstream `pi.appendEntry`), for extension state.
    pub fn append_custom_entry(&self, custom_type: &str, data: Option<Value>) {
        let appended = self.session_manager().append_custom_entry(custom_type, data);
        match appended {
            Ok(id) => {
                let entry = self.session_manager().get_entry(&id).map(|entry| Value::Object(entry.raw().clone()));
                if let Some(entry) = entry {
                    self.emit(&AgentSessionEvent::EntryAppended { entry });
                }
            }
            Err(error) => eprintln!("Warning: could not save session: {error:#}"),
        }
    }

    /// Register or remove the built-in `codemode` and `tool_search` tools as their toggles say.
    /// Both register inactive; the registry is refreshed by the caller.
    pub(crate) fn install_builtin_tools(&self) {
        let (codemode, tool_search) = {
            let builtins = lock(&self.builtins);
            (builtins.codemode.clone(), builtins.tool_search.clone())
        };
        let codemode = match (self.builtin_extension_enabled("codemode"), codemode) {
            (false, _) => None,
            (true, Some(existing)) => Some(existing),
            (true, None) => {
                let host =
                    Arc::new(codemode_host::SessionCodemodeHost::new(self.weak.clone(), self.model_runtime().clone()));
                let docs = codemode_docs_path(&self.services.agent_dir);
                let tool: SharedTool = Arc::new(SessionCodemodeTool::new(host, self.settings().clone(), &docs));
                Some(tool)
            }
        };
        let tool_search = match (self.builtin_extension_enabled("tool-search"), tool_search) {
            (false, _) => None,
            (true, Some(existing)) => Some(existing),
            (true, None) => Some(Arc::new(ToolSearchTool::new(self.weak.clone())) as SharedTool),
        };
        let (old_codemode, old_search) = {
            let mut builtins = lock(&self.builtins);
            let old = (builtins.codemode.take(), builtins.tool_search.take());
            builtins.codemode = codemode.clone();
            builtins.tool_search = tool_search.clone();
            old
        };
        let mut tools = lock(&self.tools);
        for (old, current, extension) in
            [(old_codemode, codemode, "codemode"), (old_search, tool_search, "tool-search")]
        {
            match (old, current) {
                (Some(old), None) => {
                    if tools.registered_tool(old.name()).is_some_and(|registered| same_tool(registered, &old)) {
                        tools.unregister(old.name());
                    }
                }
                (old, Some(current)) if old.as_ref().is_none_or(|old| !same_tool(old, &current)) => {
                    tools.register(vec![current], &ToolSource::builtin(extension));
                }
                _ => {}
            }
        }
    }

    /// Start the built-in extensions after the session's tools are in place.
    pub(crate) fn start_builtin_extensions(&self) {
        self.start_mcp();
    }

    /// `/reload` for tools: rebuild built-in tools and extensions from the reloaded settings and
    /// resources, keep the active tools (pending until they register again, as MCP tools are),
    /// activate tools `defaultTools` newly lists, and restart MCP.
    pub(crate) fn reload_tools(&self, previous_defaults: &[String]) {
        let old_mcp = self.stop_mcp_keeping_loadout();
        if let Some(manager) = old_mcp
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move { manager.shutdown().await });
        }
        let added: Vec<String> =
            self.current_default_tools().into_iter().filter(|name| !previous_defaults.contains(name)).collect();
        let builtins = super::tools::builtin_tools(self.cwd(), self.settings());
        self.install_builtin_tools();
        {
            let mut tools = lock(&self.tools);
            let mut active = tools.registry.active_names();
            for name in &active {
                if !tools.pending.contains(name) {
                    tools.pending.push(name.clone());
                }
            }
            active.extend(added);
            tools.set_builtin(builtins);
            tools.refresh(ActiveSelection { names: Some(active), include_all_registered: true });
        }
        self.sync_agent_tools();
        self.start_mcp();
    }

    /// Stop MCP for a reload: its active tools become pending before they are dropped.
    fn stop_mcp_keeping_loadout(&self) -> Option<McpManager> {
        {
            let mut tools = lock(&self.tools);
            for name in tools.registry.active_names() {
                if !tools.pending.contains(&name) {
                    tools.pending.push(name);
                }
            }
        }
        self.stop_mcp()
    }
}
