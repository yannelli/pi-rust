//! Servers extensions register (upstream `registeredServers`): an `mcp.json` server with the same namespace,
//! or an ACP client server with the same [`server_key`], takes precedence. Changes apply to the session only.

use std::sync::Arc;

use super::{Inner, McpManager, Server};
use crate::config::{McpConfigScope, McpServerEntry, mcp_namespace};
use crate::naming::server_key;
use crate::registered::McpServerRegistry;
use crate::runtime::McpServerConnection;

/// The registered servers as entries with their registered config, except the ones `mcp.json` or an
/// ACP client server (`clients`) overrides, which are described in the second list.
fn registered_servers(
    registry: &McpServerRegistry,
    configured: &[(String, String)],
    clients: &[String],
) -> (Vec<(McpServerEntry, String)>, Vec<String>) {
    let mut servers = Vec::new();
    let mut overridden = Vec::new();
    for server in registry.list() {
        let key = server_key(&server.name);
        if let Some(client) = clients.iter().find(|client| server_key(client) == key) {
            overridden.push(format!(
                "\"{}\" registered by {} is overridden by the ACP client server \"{client}\"",
                server.name, server.extension_path
            ));
            continue;
        }
        let namespace = mcp_namespace(&server.name);
        if let Some((name, source)) = configured.iter().find(|(name, _)| mcp_namespace(name) == namespace) {
            overridden.push(format!(
                "\"{}\" registered by {} is overridden by \"{name}\" in {source}",
                server.name, server.extension_path
            ));
            continue;
        }
        let registered = serde_json::to_string(&server.config).unwrap_or_default();
        let mut entry = McpServerEntry::new(server.name, server.config, server.extension_path);
        entry.scope = Some(McpConfigScope::Extension);
        servers.push((entry, registered));
    }
    (servers, overridden)
}

impl McpManager {
    /// Registered servers that `mcp.json` overrides, as `/mcp` lists them.
    pub fn overridden(&self) -> Vec<String> {
        self.inner.lock().overridden.clone()
    }

    /// What `/mcp` shows above the servers: config errors and overridden registered servers.
    pub fn notices(&self) -> Vec<String> {
        let state = self.inner.lock();
        let errors = state.config_errors.iter().map(|error| format!("config: {error}"));
        errors.chain(state.overridden.iter().map(|line| format!("overridden: {line}"))).collect()
    }

    /// Connect servers registered since the last call and close the ones unregistered or
    /// registered again with another config. The manager calls it when its registry changes.
    pub async fn sync_registered_servers(&self) {
        let Some(registry) = self.inner.options.registry.clone() else { return };
        let (removed, added, session) = {
            let mut state = self.inner.lock();
            if state.session.is_cancelled() {
                return;
            }
            let (registered, overridden) = registered_servers(&registry, &state.configured, &state.clients);
            state.overridden = overridden;
            let next = |name: &str| registered.iter().find(|(entry, _)| entry.name == name).map(|(_, config)| config);
            let (kept, removed): (Vec<Server>, Vec<Server>) =
                std::mem::take(&mut state.servers).into_iter().partition(|server| {
                    !server.registered() || next(&server.entry.name) == server.registered_config.as_ref()
                });
            state.servers = kept;
            let mut added = Vec::new();
            for (entry, config) in registered {
                if state.named(&entry.name).is_none() {
                    state.next_id += 1;
                    let server = Server::new(state.next_id, entry, Some(config));
                    added.push((server.id, server.enabled()));
                    state.servers.push(server);
                }
            }
            (removed, added, state.session.clone())
        };
        for server in &removed {
            self.inner.hide_tools(&server.entry.name);
        }
        self.inner.emit_change();
        if let Some(changed) = &self.inner.options.on_registered_change {
            changed();
        }
        let closing = removed.iter().filter_map(|server| server.connection.as_ref()).map(McpServerConnection::close);
        futures::future::join_all(closing).await;
        let connecting: Vec<u64> = added.into_iter().filter(|(_, enabled)| *enabled).map(|(id, _)| id).collect();
        if session.is_cancelled() || connecting.is_empty() {
            return;
        }
        futures::future::join_all(connecting.iter().map(|id| self.inner.start_connection(*id, None))).await;
        if session.is_cancelled() {
            return;
        }
        self.inner.report_problems(Some(&connecting));
    }
}

impl Inner {
    /// The registered servers for a new session, and the overridden notices.
    pub(super) fn registered_for_start(
        &self,
        configured: &[(String, String)],
        clients: &[String],
    ) -> (Vec<(McpServerEntry, String)>, Vec<String>) {
        match &self.options.registry {
            Some(registry) => registered_servers(registry, configured, clients),
            None => (Vec::new(), Vec::new()),
        }
    }

    /// Follow registry changes for the session, replacing an earlier subscription.
    pub(super) fn watch_registry(self: &Arc<Self>) {
        let Some(registry) = self.options.registry.clone() else { return };
        self.unwatch_registry();
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        let weak = Arc::downgrade(self);
        let id = registry.subscribe(move || {
            let Some(inner) = weak.upgrade() else { return };
            let manager = McpManager { inner };
            handle.spawn(async move { manager.sync_registered_servers().await });
        });
        self.lock().registry_listener = Some(id);
    }

    pub(super) fn unwatch_registry(&self) {
        let id = self.lock().registry_listener.take();
        if let (Some(registry), Some(id)) = (&self.options.registry, id) {
            registry.unsubscribe(id);
        }
    }
}
