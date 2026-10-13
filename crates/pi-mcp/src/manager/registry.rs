//! The tools MCP registers. Tools cannot be unregistered, so tools a server dropped, or tools of a
//! disabled server, are registered again as hidden. Names stay with the tool they were given to.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Weak};

use pi_agent::{SharedTool, ToolUpdateCallback};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{CODEMODE_TOOL_NAME, Inner, McpManager, NotifyLevel, TOOL_SEARCH_TOOL_NAME};
use crate::config::{McpExposure, get_mcp_tool_exposure, mcp_namespace};
use crate::naming::create_mcp_tool_name;
use crate::output::McpToolOutput;
use crate::resources::{McpResourceServer, McpResourceTool, ResourceServersFn, create_mcp_resource_tools};
use crate::runtime::{McpServerConnection, ServerState};
use crate::section::configured_exposures;
use crate::tools::{CallerFn, McpTool, McpToolCaller, McpToolOptions, ToolAnnotations, ToolExposure, ToolNamespace};

/// A registered MCP tool: a server tool or a resource tool.
#[derive(Clone)]
pub enum McpAgentTool {
    Tool(Arc<McpTool>),
    Resource(Arc<McpResourceTool>),
}

impl McpAgentTool {
    pub fn name(&self) -> &str {
        match self {
            Self::Tool(tool) => pi_agent::AgentTool::name(tool.as_ref()),
            Self::Resource(tool) => pi_agent::AgentTool::name(tool.as_ref()),
        }
    }

    pub fn exposure(&self) -> ToolExposure {
        match self {
            Self::Tool(tool) => tool.exposure(),
            Self::Resource(tool) => tool.exposure(),
        }
    }

    pub fn agent_tool(&self) -> SharedTool {
        match self {
            Self::Tool(tool) => tool.clone(),
            Self::Resource(tool) => tool.clone(),
        }
    }

    pub fn annotations(&self) -> Option<ToolAnnotations> {
        match self {
            Self::Tool(tool) => tool.annotations().cloned(),
            Self::Resource(tool) => Some(tool.annotations()),
        }
    }

    pub fn output_schema(&self) -> Value {
        match self {
            Self::Tool(tool) => tool.output_schema().clone(),
            Self::Resource(tool) => tool.output_schema(),
        }
    }

    pub fn namespace(&self) -> Option<&ToolNamespace> {
        match self {
            Self::Tool(tool) => Some(tool.namespace()),
            Self::Resource(_) => None,
        }
    }

    /// Call the tool for a codemode script, which gets `structured_content` whole.
    pub async fn call(
        &self,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateCallback>,
    ) -> anyhow::Result<McpToolOutput> {
        match self {
            Self::Tool(tool) => tool.call(args, cancel, on_update).await,
            Self::Resource(tool) => tool.call(args, cancel).await,
        }
    }
}

#[derive(Default)]
pub(super) struct Registry {
    /// Tool name to the `<server>\0<tool>` it was given to, so names stay unique and stable.
    owners: HashMap<String, String>,
    /// Tool names currently offered by each server.
    server_tools: HashMap<String, Vec<String>>,
    /// Last definition registered under each name, to register withdrawn tools again as hidden.
    definitions: HashMap<String, Arc<McpTool>>,
    /// Registered tools in registration order; registering a name again replaces it.
    registered: Vec<McpAgentTool>,
    /// Exposure the resource tools were last registered with.
    resource_exposure: Option<McpExposure>,
}

impl Registry {
    fn register(&mut self, tool: McpAgentTool) {
        match self.registered.iter_mut().find(|registered| registered.name() == tool.name()) {
            Some(slot) => *slot = tool,
            None => self.registered.push(tool),
        }
    }

    /// The server owning a tool name.
    pub(super) fn owner(&self, name: &str) -> Option<String> {
        self.owners.get(name).and_then(|owner| owner.split('\0').next()).map(str::to_string)
    }
}

impl McpManager {
    /// The registered tools with their current exposure. Direct tools belong in the model's tools.
    pub fn tools(&self) -> Vec<McpAgentTool> {
        self.inner.lock().registry.registered.clone()
    }

    pub fn tool(&self, name: &str) -> Option<McpAgentTool> {
        self.tools().into_iter().find(|tool| tool.name() == name)
    }

    /// Tools reached through codemode or tool_search need one of them active. Returns the tools to
    /// activate (codemode for `codemode` unless `autoEnableCodemode` is false, tool_search for
    /// `deferred`) and warns once when MCP tools stay unreachable.
    pub fn ensure_discovery_active(&self, has_codemode: bool, has_tool_search: bool, active: &[String]) -> Vec<String> {
        let (exposures, auto_enable) = {
            let state = self.inner.lock();
            let enabled = state.servers.iter().filter(|server| server.enabled());
            let exposures: BTreeSet<McpExposure> =
                enabled.flat_map(|server| configured_exposures(&server.entry)).collect();
            (exposures, state.auto_enable_codemode)
        };
        let needs_codemode = exposures.contains(&McpExposure::Codemode);
        let needs_tool_search = exposures.contains(&McpExposure::Deferred);
        if !needs_codemode && !needs_tool_search {
            return Vec::new();
        }
        let is_active = |name: &str| active.iter().any(|tool| tool == name);
        let mut activate = Vec::new();
        if needs_codemode && has_codemode && auto_enable && !is_active(CODEMODE_TOOL_NAME) {
            activate.push(CODEMODE_TOOL_NAME.to_string());
        }
        if needs_tool_search && has_tool_search && !is_active(TOOL_SEARCH_TOOL_NAME) {
            activate.push(TOOL_SEARCH_TOOL_NAME.to_string());
        }
        let reachable = |name: &str| is_active(name) || activate.iter().any(|tool| tool == name);
        if (has_codemode && reachable(CODEMODE_TOOL_NAME)) || (has_tool_search && reachable(TOOL_SEARCH_TOOL_NAME)) {
            return activate;
        }
        let mut state = self.inner.lock();
        if !state.warned_unreachable {
            state.warned_unreachable = true;
            drop(state);
            let reason =
                if needs_codemode && has_codemode && !auto_enable { " (autoEnableCodemode is false)" } else { "" };
            let message = format!(
                "MCP tools are only reachable from the codemode or tool_search tool, but neither is active{reason}; they cannot be called."
            );
            self.inner.notify(&message, NotifyLevel::Warning);
        }
        activate
    }
}

/// Resolve the server when a call runs: a prepared call can outlive the connection it was made for.
fn caller(weak: Weak<Inner>, server: String, tool: String) -> CallerFn {
    Arc::new(move || {
        let disabled = || format!("MCP server \"{server}\" is disabled.");
        let inner = weak.upgrade().ok_or_else(disabled)?;
        let state = inner.lock();
        let current = state.named(&server).filter(|current| current.enabled()).ok_or_else(disabled)?;
        let connection =
            current.connection.clone().ok_or_else(|| format!("MCP server \"{server}\" is still starting."))?;
        let dropped = connection.state() == ServerState::Connected
            && !connection.tools().iter().any(|offered| offered.name == tool);
        if get_mcp_tool_exposure(&current.entry.config, &tool) == McpExposure::Hidden || dropped {
            return Err(format!("MCP tool \"{server}/{tool}\" is no longer available."));
        }
        Ok(Arc::new(connection) as Arc<dyn McpToolCaller>)
    })
}

impl Inner {
    /// Enabled servers with resources whose exposure is not `hidden`: the servers the resource tools reach.
    fn resource_servers(&self) -> Vec<(McpExposure, McpServerConnection)> {
        let state = self.lock();
        let servers = state.servers.iter().filter(|server| server.enabled());
        servers
            .filter(|server| server.entry.config.exposure() != McpExposure::Hidden)
            .filter_map(|server| Some((server.entry.config.exposure(), server.connection.clone()?)))
            .filter(|(_, connection)| connection.has_resources())
            .collect()
    }

    pub(super) fn register_tools(self: &Arc<Self>, connection: &McpServerConnection) {
        let server = connection.name().to_string();
        let (offered, instructions, timeout) = (connection.tools(), connection.instructions(), connection.timeout());
        {
            let mut state = self.lock();
            let entry =
                state.named(&server).map_or_else(|| connection.entry().clone(), |current| current.entry.clone());
            let description = entry.config.description.as_deref().map(str::trim).filter(|text| !text.is_empty());
            let namespace = ToolNamespace {
                name: mcp_namespace(&server),
                description: description.map(str::to_string),
                instructions,
            };
            let registry = &mut state.registry;
            let previous = registry.server_tools.remove(&server).unwrap_or_default();
            let mut current: Vec<String> = Vec::new();
            // All tools whose names sanitize to the same name get the hash suffix, so which one keeps
            // the plain name does not depend on the order of the list.
            let mut names: Vec<&str> = Vec::new();
            for tool in &offered {
                if !names.contains(&tool.name.as_str()) {
                    names.push(&tool.name);
                }
            }
            let plain: Vec<String> = names.iter().map(|tool| create_mcp_tool_name(&server, tool, |_| false)).collect();
            for tool in offered {
                let owner = format!("{server}\0{}", tool.name);
                let name = create_mcp_tool_name(&server, &tool.name, |candidate| {
                    registry.owners.get(candidate).is_some_and(|existing| *existing != owner)
                        || current.iter().any(|name| name == candidate)
                        || plain.iter().filter(|name| *name == candidate).count() > 1
                });
                registry.owners.insert(name.clone(), owner);
                current.push(name.clone());
                let readable = Arc::downgrade(self);
                let readable_server = server.clone();
                let definition = Arc::new(McpTool::new(McpToolOptions {
                    server: server.clone(),
                    exposure: get_mcp_tool_exposure(&entry.config, &tool.name),
                    caller: caller(Arc::downgrade(self), server.clone(), tool.name.clone()),
                    tool,
                    name: name.clone(),
                    namespace: namespace.clone(),
                    timeout,
                    readable_resources: Some(Arc::new(move || {
                        let Some(inner) = readable.upgrade() else { return false };
                        inner.resource_servers().iter().any(|(_, connection)| connection.name() == readable_server)
                    })),
                }));
                registry.definitions.insert(name, definition.clone());
                registry.register(McpAgentTool::Tool(definition));
            }
            for name in previous.iter().filter(|name| !current.contains(name)) {
                if let Some(definition) = registry.definitions.get(name).cloned() {
                    registry.register(McpAgentTool::Tool(Arc::new(definition.with_exposure(ToolExposure::Hidden))));
                }
            }
            registry.server_tools.insert(server, current);
        }
        self.sync_resource_tools();
        self.emit_change();
    }

    /// Make a disabled server's tools unreachable.
    pub(super) fn hide_tools(self: &Arc<Self>, server: &str) {
        {
            let mut state = self.lock();
            let registry = &mut state.registry;
            for name in registry.server_tools.insert(server.into(), Vec::new()).unwrap_or_default() {
                if let Some(definition) = registry.definitions.get(&name).cloned() {
                    registry.register(McpAgentTool::Tool(Arc::new(definition.with_exposure(ToolExposure::Hidden))));
                }
            }
        }
        self.sync_resource_tools();
    }

    /// Register the resource tools with the widest exposure of the servers they reach: `direct` when
    /// one of them is direct, and so on. They are hidden when no server has resources.
    pub(super) fn sync_resource_tools(self: &Arc<Self>) {
        let exposures: BTreeSet<McpExposure> =
            self.resource_servers().into_iter().map(|(exposure, _)| exposure).collect();
        let widest = [McpExposure::Direct, McpExposure::Codemode, McpExposure::Deferred];
        let next = widest.into_iter().find(|exposure| exposures.contains(exposure)).unwrap_or(McpExposure::Hidden);
        let mut state = self.lock();
        let current = state.registry.resource_exposure;
        if current == Some(next) || (current.is_none() && next == McpExposure::Hidden) {
            return;
        }
        state.registry.resource_exposure = Some(next);
        let weak = Arc::downgrade(self);
        let servers: ResourceServersFn = Arc::new(move || {
            let Some(inner) = weak.upgrade() else { return Vec::new() };
            let servers = inner.resource_servers().into_iter();
            servers.map(|(_, connection)| Arc::new(connection) as Arc<dyn McpResourceServer>).collect()
        });
        for tool in create_mcp_resource_tools(next, servers) {
            state.registry.register(McpAgentTool::Resource(Arc::new(tool)));
        }
    }
}
