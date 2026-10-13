//! Extensions and the session they act on: loading the extensions resources list, the registry
//! snapshot and the tools it registers, MCP servers, and event emission.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use pi_agent::SharedTool;
use serde_json::{Value, json};

use super::inline::InlineExtension;
use super::loader::{self, LoadEntry};
use super::registry::Registry;
use super::runner::{ExtensionError, ExtensionRunner};
use super::tool::ExtensionTool;
use crate::agent_session::{AgentSessionServices, RuntimeDiagnostic, ToolSource, lock};

/// What [`load_extensions`] loads besides the resources list.
#[derive(Clone, Default)]
pub struct LoadOptions {
    /// Values of extension flags (`--name value` on the command line).
    pub flag_values: BTreeMap<String, Value>,
    /// Extensions supplied by the host program, loaded before the files.
    pub inline: Vec<Arc<dyn InlineExtension>>,
    /// Files to load in place of the resources list.
    pub entries: Option<Vec<LoadEntry>>,
}

#[derive(Default)]
pub struct LoadedExtensions {
    /// `None` when there is nothing to load.
    pub runner: Option<Arc<ExtensionRunner>>,
    pub diagnostics: Vec<RuntimeDiagnostic>,
}

/// Load the extensions of `services` (`-e`, settings, packages, `.pi/extensions` when trusted)
/// and register their providers; attach the runner to the session created next.
pub async fn load_extensions(services: &AgentSessionServices, options: &LoadOptions) -> LoadedExtensions {
    let entries = match &options.entries {
        Some(entries) => entries.clone(),
        None => loader::load_entries(&services.resources().resources().extensions),
    };
    if entries.is_empty() && options.inline.is_empty() {
        return LoadedExtensions::default();
    }
    let runner = match ExtensionRunner::start(&services.cwd, &services.agent_dir, services.model_runtime.clone()).await
    {
        Ok(runner) => runner,
        Err(error) => {
            let message = format!("Failed to start the extension host: {error:#}");
            return LoadedExtensions { runner: None, diagnostics: vec![RuntimeDiagnostic::error(message)] };
        }
    };
    runner.load_inline(&options.inline).await;
    runner.load_entries(&entries).await;
    for (name, value) in &options.flag_values {
        let _ = runner.call_json("setFlag", vec![json!(name), value.clone()]).await;
    }
    let _ = runner.call_json("bindModels", Vec::new()).await;
    let diagnostics = runner
        .load_errors()
        .into_iter()
        .map(|error| RuntimeDiagnostic::error(format!("Failed to load extension \"{}\": {}", error.path, error.error)))
        .collect();
    LoadedExtensions { runner: Some(runner), diagnostics }
}

impl ExtensionRunner {
    /// Store the registry JavaScript published; tool changes reach the bound session.
    pub(crate) fn apply_registry(&self, value: Value) {
        let Ok(registry) = serde_json::from_value::<Registry>(value) else { return };
        let tools_changed = {
            let mut current = self.registry.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            let changed = current.tools != registry.tools;
            *current = registry;
            changed
        };
        if tools_changed {
            self.sync_tools();
        }
    }

    /// Register the tools of every extension with the session, replacing earlier ones.
    pub(crate) fn sync_tools(&self) {
        let Some(session) = self.session() else { return };
        let registry = self.registry().clone();
        let mut by_path: BTreeMap<String, Vec<SharedTool>> = BTreeMap::new();
        let this = self.this();
        for descriptor in &registry.tools {
            let tool: SharedTool = Arc::new(ExtensionTool::new(&this, descriptor.clone()));
            by_path.entry(descriptor.extension_path.clone()).or_default().push(tool);
        }
        for (path, tools) in self.inline_tools() {
            by_path.entry(path).or_default().extend(tools);
        }
        let paths: BTreeSet<String> = {
            let mut known = lock(&self.tool_paths);
            known.extend(by_path.keys().cloned());
            known.clone()
        };
        for path in paths {
            let tools = by_path.remove(&path).unwrap_or_default();
            let source = ToolSource::extension(path.clone(), registry.source_of(&path));
            session.replace_source_tools(tools, &source);
        }
    }

    /// Remove this runner's tools from its session, before it shuts down for a reload.
    pub(crate) fn remove_tools(&self) {
        let Some(session) = self.session() else { return };
        let registry = self.registry().clone();
        let paths = std::mem::take(&mut *lock(&self.tool_paths));
        for path in paths {
            let source = ToolSource::extension(path.clone(), registry.source_of(&path));
            session.replace_source_tools(Vec::new(), &source);
        }
    }

    /// Run the handlers of `event`; the result is what `session_before_*` handlers returned.
    pub async fn emit(&self, event: Value) -> Value {
        let kind = event["type"].as_str().unwrap_or_default().to_string();
        if !self.has_handlers(&kind) {
            return Value::Null;
        }
        self.call_json("emit", vec![event]).await.unwrap_or(Value::Null)
    }

    /// Emit without waiting, for events upstream does not await.
    pub fn emit_detached(&self, event: Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        if !self.has_handlers(kind) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        // One queue keeps the order events happened in, as upstream's single event loop does.
        let sender = self.detached.get_or_init(|| {
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
            let weak = Arc::downgrade(&self.this());
            handle.spawn(async move {
                while let Some(event) = receiver.recv().await {
                    let Some(runner) = weak.upgrade() else { break };
                    runner.emit(event).await;
                }
            });
            sender
        });
        let _ = sender.send(event);
    }

    /// Servers registered while nothing connects them become extension errors.
    pub(crate) fn report_unconnected_mcp(&self) {
        if self.has_handlers("mcp_servers_change") {
            return;
        }
        if self.session().is_some_and(|session| session.builtin_extension_enabled("mcp")) {
            return;
        }
        for (extension_path, error) in self.mcp.take_unconnected_reports() {
            self.emit_error(ExtensionError { extension_path, event: "register_mcp_server".into(), error, stack: None });
        }
    }

    /// `mcp_servers_change` after every registry change, once bound.
    pub(crate) fn watch_mcp_servers(&self) {
        let weak = self.weak();
        self.mcp.subscribe(move || {
            let Some(runner) = weak.upgrade() else { return };
            if runner.session().is_none() {
                return;
            }
            runner.emit_detached(json!({"type": "mcp_servers_change", "servers": runner.mcp_servers_json()}));
            runner.report_unconnected_mcp();
        });
    }

    /// `pi.getMcpServers()`.
    pub(crate) fn mcp_servers_json(&self) -> Value {
        let servers: Vec<Value> = self
            .mcp
            .list()
            .into_iter()
            .map(|server| json!({"name": server.name, "config": server.config, "extensionPath": server.extension_path}))
            .collect();
        Value::Array(servers)
    }
}
