//! MCP servers that extensions register (upstream `McpServerRegistry` and the checks of
//! `pi.registerMcpServer()`). The registry only validates and stores them; [`crate::McpManager`]
//! connects them next to the servers from `mcp.json`, which take precedence.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::config::{McpServerConfig, mcp_namespace, validate_mcp_server_config};

/// A server an extension registered.
#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredMcpServer {
    pub name: String,
    pub config: McpServerConfig,
    /// Path of the extension that registered the server.
    pub extension_path: String,
}

type ChangeListener = Arc<dyn Fn() + Send + Sync>;

static NEXT_LISTENER: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct RegistryState {
    /// In registration order; registering a name again keeps its place.
    servers: Vec<RegisteredMcpServer>,
    listeners: Vec<(u64, ChangeListener)>,
    /// Servers already reported as registered without an extension that connects them.
    reported: HashSet<String>,
}

/// The servers registered by the extensions of one runtime. Clones share it.
#[derive(Clone, Default)]
pub struct McpServerRegistry {
    state: Arc<Mutex<RegistryState>>,
}

impl McpServerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register or replace a server for `extension_path`, with the same config as an `mcpServers`
    /// entry. Fails for invalid configs, names another extension registered, and names that share
    /// a namespace with another registered server.
    pub fn register(&self, extension_path: &str, name: &str, config: &Value) -> Result<(), String> {
        let config = validate_mcp_server_config(name, config)
            .map_err(|error| format!("Invalid MCP server registered by extension \"{extension_path}\": {error}"))?;
        {
            let mut state = self.lock();
            let existing = state.servers.iter().position(|server| server.name == name);
            if let Some(owner) = existing.map(|index| &state.servers[index].extension_path)
                && owner != extension_path
            {
                return Err(format!("MCP server \"{name}\" is already registered by extension \"{owner}\""));
            }
            // Names that differ only in `-` and `_` would share a namespace.
            let namespace = mcp_namespace(name);
            let clash =
                state.servers.iter().find(|server| server.name != name && mcp_namespace(&server.name) == namespace);
            if let Some(clash) = clash {
                return Err(format!("MCP server \"{name}\" conflicts with registered server \"{}\"", clash.name));
            }
            let server = RegisteredMcpServer { name: name.into(), config, extension_path: extension_path.into() };
            match existing {
                Some(index) => state.servers[index] = server,
                None => state.servers.push(server),
            }
        }
        self.emit_change();
        Ok(())
    }

    /// Remove a server `extension_path` registered. Servers of other extensions are left alone.
    pub fn unregister(&self, name: &str, extension_path: &str) {
        {
            let mut state = self.lock();
            let before = state.servers.len();
            state.servers.retain(|server| server.name != name || server.extension_path != extension_path);
            if state.servers.len() == before {
                return;
            }
        }
        self.emit_change();
    }

    pub fn get(&self, name: &str) -> Option<RegisteredMcpServer> {
        self.lock().servers.iter().find(|server| server.name == name).cloned()
    }

    /// The registered servers, in registration order.
    pub fn list(&self) -> Vec<RegisteredMcpServer> {
        self.lock().servers.clone()
    }

    /// Called after every change (upstream emits `mcp_servers_change` from it). Returns an id for
    /// [`Self::unsubscribe`].
    pub fn subscribe(&self, listener: impl Fn() + Send + Sync + 'static) -> u64 {
        let id = NEXT_LISTENER.fetch_add(1, Ordering::Relaxed);
        self.lock().listeners.push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        self.lock().listeners.retain(|(listener, _)| *listener != id);
    }

    /// For runtimes where no extension connects MCP servers: each registered server not reported
    /// before, with the extension that registered it and the error to report once.
    pub fn take_unconnected_reports(&self) -> Vec<(String, String)> {
        let mut state = self.lock();
        let servers = state.servers.clone();
        servers
            .into_iter()
            .filter(|server| state.reported.insert(server.name.clone()))
            .map(|server| {
                let message = format!(
                    "MCP server \"{}\" is registered, but no loaded extension connects MCP servers; another extension may have replaced the built-in MCP support",
                    server.name
                );
                (server.extension_path, message)
            })
            .collect()
    }

    fn emit_change(&self) {
        let listeners: Vec<ChangeListener> =
            self.lock().listeners.iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener();
        }
    }
}
