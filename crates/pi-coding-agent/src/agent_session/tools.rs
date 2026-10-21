//! The tool registry of a session (upstream `_refreshToolRegistry`, `_applyToolLoadout`, and
//! `_rebuildSystemPrompt`): registered tools, the active set declared to the model, the callable
//! set other tools reach, pending tools of a restored loadout, and their prompt contributions.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Weak};

use pi_agent::{
    ActivationFilter, RefreshOptions, SharedTool, ToolAnnotations, ToolExposure, ToolNamespace, ToolRegistry,
};
use pi_mcp::naming::is_mcp_tool_name;
use serde_json::Value;

use crate::resources::LoadedResources;
use crate::settings::SettingsManager;
use crate::settings::tools::{apply_tool_modifiers, matches_any_tool};
use crate::system_prompt::BuildSystemPromptOptions;
use crate::tools::{self, ALL_TOOLS, SessionEnvironmentFn, ToolsOptions};

use super::AgentSession;

/// Where a registered tool comes from (upstream `sourceInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolSource {
    /// `builtin:<name>` for built-in tools and extensions, an extension path, or `<sdk:name>`.
    pub path: String,
    /// `builtin`, `sdk`, or the source of the extension, such as `local` or a package source.
    pub source: String,
}

impl ToolSource {
    pub fn builtin(name: &str) -> Self {
        Self { path: format!("builtin:{name}"), source: "builtin".into() }
    }

    pub fn sdk(name: &str) -> Self {
        Self { path: format!("<sdk:{name}>"), source: "sdk".into() }
    }

    pub fn extension(path: impl Into<String>, source: impl Into<String>) -> Self {
        Self { path: path.into(), source: source.into() }
    }
}

/// A registered tool as `getAllTools` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_guidelines: Vec<String>,
    pub exposure: ToolExposure,
    pub namespace: Option<ToolNamespace>,
    pub annotations: Option<ToolAnnotations>,
    /// The tool declares which tools a call reaches (`AgentTool::reaches_tools`).
    pub reaches_tools: bool,
    /// `builtin`, `sdk`, or the source of the registering extension.
    pub source: String,
    /// `builtin:<name>`, `<sdk:name>`, or the path of the registering extension.
    pub source_path: String,
}

/// How [`ToolState::refresh`] picks the active tools.
#[derive(Clone, Debug, Default)]
pub(crate) struct ActiveSelection {
    /// The active set to apply. Unset keeps the current set and adds tools that registration
    /// activates for the first time.
    pub names: Option<Vec<String>>,
    /// Activate every registered extension and SDK tool that registration activates.
    pub include_all_registered: bool,
}

pub(crate) struct ToolState {
    pub registry: ToolRegistry,
    builtin: Vec<SharedTool>,
    /// Tools of built-in extensions and extensions, in registration order.
    registered: Vec<(SharedTool, ToolSource)>,
    /// SDK tools; they win over other tools of the same name.
    custom: Vec<SharedTool>,
    /// `--tools` names or patterns; `None` allows every tool.
    allowed: Option<Vec<String>>,
    /// Whether the allowlist filters MCP tools: it is empty or names an `mcp__` tool.
    allowlist_filters_mcp: bool,
    excluded: Vec<String>,
    /// Tools of the restored or reloaded loadout that are not registered yet.
    pub pending: Vec<String>,
    /// Whether `tool_search` was registered when the activation filter was built.
    filter_has_tool_search: Option<bool>,
    pub uses_default_tools: bool,
    pub default_tool_modifiers: Vec<String>,
    /// Prompt options from the resources; the tool fields follow the registry.
    pub base_prompt: BuildSystemPromptOptions,
    /// SDK options of the built-in tools; unset uses the settings.
    pub tool_options: Option<ToolsOptions>,
}

/// The `--tools` and `--exclude-tools` lists and the `defaultTools` handling of a session.
pub(crate) struct ToolSelection {
    pub allowed: Option<Vec<String>>,
    pub excluded: Vec<String>,
    pub uses_default_tools: bool,
    pub default_tool_modifiers: Vec<String>,
}

impl ToolState {
    pub fn new(
        builtin: Vec<SharedTool>,
        custom: Vec<SharedTool>,
        selection: ToolSelection,
        base_prompt: BuildSystemPromptOptions,
    ) -> Self {
        let allowlist_filters_mcp = selection
            .allowed
            .as_ref()
            .is_some_and(|allowed| allowed.is_empty() || allowed.iter().any(|entry| entry.starts_with("mcp__")));
        Self {
            registry: ToolRegistry::default(),
            builtin,
            registered: Vec::new(),
            custom,
            allowed: selection.allowed,
            allowlist_filters_mcp,
            excluded: selection.excluded,
            pending: Vec::new(),
            filter_has_tool_search: None,
            uses_default_tools: selection.uses_default_tools,
            default_tool_modifiers: selection.default_tool_modifiers,
            base_prompt,
            tool_options: None,
        }
    }

    /// Whether `--tools` and `--exclude-tools` keep the tool registered. MCP tools stay registered
    /// unless the allowlist filters them.
    pub fn is_allowed(&self, name: &str) -> bool {
        if matches_any_tool(&self.excluded, name) {
            return false;
        }
        match &self.allowed {
            None => true,
            Some(allowed) => matches_any_tool(allowed, name) || (!self.allowlist_filters_mcp && is_mcp_tool_name(name)),
        }
    }

    /// Registered and SDK tools, which registration activates as a group at startup.
    fn extension_tools(&self) -> impl Iterator<Item = &SharedTool> {
        self.registered.iter().map(|(tool, _)| tool).chain(&self.custom)
    }

    /// The allowed tools in registration order: built-in, registered, then SDK tools.
    fn allowed_tools(&self) -> Vec<SharedTool> {
        let all = self.builtin.iter().chain(self.extension_tools());
        all.filter(|tool| self.is_allowed(tool.name())).cloned().collect()
    }

    /// Whether `name` is a built-in tool (upstream `_baseToolDefinitions`), allowed or not.
    pub fn is_builtin(&self, name: &str) -> bool {
        self.builtin.iter().any(|tool| tool.name() == name)
    }

    pub fn set_builtin(&mut self, tools: Vec<SharedTool>) {
        self.builtin = tools;
    }

    /// Register tools, replacing ones with the same name in place.
    pub fn register(&mut self, tools: Vec<SharedTool>, source: &ToolSource) {
        for tool in tools {
            match self.registered.iter_mut().find(|(existing, _)| existing.name() == tool.name()) {
                Some(slot) => *slot = (tool, source.clone()),
                None => self.registered.push((tool, source.clone())),
            }
        }
    }

    /// Replace every tool registered from `source_path` with `tools`, at the place of the first.
    pub fn replace_source(&mut self, source_path: &str, tools: Vec<SharedTool>, source: &ToolSource) {
        let index = self.registered.iter().position(|(_, existing)| existing.path == source_path);
        self.registered.retain(|(_, existing)| existing.path != source_path);
        let names: HashSet<String> = tools.iter().map(|tool| tool.name().to_string()).collect();
        self.registered.retain(|(tool, _)| !names.contains(tool.name()));
        let at = index.unwrap_or(self.registered.len()).min(self.registered.len());
        let entries = tools.into_iter().map(|tool| (tool, source.clone()));
        self.registered.splice(at..at, entries);
    }

    pub fn unregister(&mut self, name: &str) -> bool {
        let before = self.registered.len();
        self.registered.retain(|(tool, _)| tool.name() != name);
        self.registered.len() != before
    }

    pub fn source_of(&self, name: &str) -> ToolSource {
        if self.custom.iter().any(|tool| tool.name() == name) {
            return ToolSource::sdk(name);
        }
        match self.registered.iter().find(|(tool, _)| tool.name() == name) {
            Some((_, source)) => source.clone(),
            None => ToolSource::builtin(name),
        }
    }

    pub fn registered_tool(&self, name: &str) -> Option<&SharedTool> {
        self.registered.iter().find(|(tool, _)| tool.name() == name).map(|(tool, _)| tool)
    }

    fn update_activation_filter(&mut self, has_tool_search: bool) {
        if self.filter_has_tool_search == Some(has_tool_search) {
            return;
        }
        self.filter_has_tool_search = Some(has_tool_search);
        let filter: Option<ActivationFilter> = self.allowed.clone().map(|allowed| {
            let filter: ActivationFilter = Arc::new(move |name: &str, exposure: ToolExposure| {
                matches_any_tool(&allowed, name)
                    || !is_mcp_tool_name(name)
                    || (exposure != ToolExposure::Direct && has_tool_search)
            });
            filter
        });
        self.registry.set_activation_filter(filter);
    }

    /// Rebuild the registry from the allowed tools and apply the active set (upstream
    /// `_refreshToolRegistry`). Pending tools that are registered now become active.
    pub fn refresh(&mut self, selection: ActiveSelection) {
        let activated_before: HashSet<String> = self.registry.default_active_names().into_iter().collect();
        let tools = self.allowed_tools();
        let explicit = selection.names.is_some();
        let mut next: Vec<String> = selection.names.unwrap_or_else(|| self.registry.active_names());
        next.retain(|name| self.is_allowed(name));
        let activates = |tool: &SharedTool| tool.exposure().is_declarable() && tool.default_active();
        if let Some(allowed) = &self.allowed {
            // Naming or matching a tool activates it even when it is not active by default.
            let named = tools.iter().filter(|tool| matches_any_tool(allowed, tool.name()));
            next.extend(named.filter(|tool| tool.exposure().is_declarable()).map(|tool| tool.name().to_string()));
        } else if selection.include_all_registered {
            let registered = self.extension_tools().filter(|tool| self.is_allowed(tool.name()));
            next.extend(registered.filter(|tool| activates(tool)).map(|tool| tool.name().to_string()));
        } else if !explicit {
            let newly = tools.iter().filter(|tool| !activated_before.contains(tool.name()) && activates(tool));
            next.extend(newly.map(|tool| tool.name().to_string()));
        }
        next.extend(self.pending.iter().cloned());
        let has_tool_search = tools.iter().any(|tool| tool.name() == pi_codemode::TOOL_SEARCH_TOOL_NAME);
        self.update_activation_filter(has_tool_search);
        self.registry.refresh(tools, RefreshOptions { active_tool_names: Some(next), extra_active: Vec::new() });
        self.after_loadout();
    }

    /// Apply an active set (upstream `_setActiveTools`). Returns whether a tool was deactivated.
    pub fn set_active(&mut self, names: &[String]) -> bool {
        let previous = self.registry.active_names();
        self.registry.set_active_tools(names);
        self.after_loadout();
        let active: HashSet<String> = self.registry.active_names().into_iter().collect();
        previous.iter().any(|name| !active.contains(name))
    }

    fn after_loadout(&mut self) {
        let active: HashSet<String> = self.registry.active_names().into_iter().collect();
        self.pending.retain(|name| !active.contains(name));
    }

    /// Fill the tool fields of prompt options: the active tools, hidden declarations, and the
    /// snippets and guidelines of every registered tool.
    pub fn apply_to_prompt(&self, options: &mut BuildSystemPromptOptions) {
        options.selected_tools = self.registry.active_names();
        options.hidden_tools = self.registry.hidden_declarations().to_vec();
        options.tool_snippets = self.registry.prompt_snippets().into_iter().collect();
        options.tool_guidelines = self.registry.prompt_guidelines();
    }

    pub fn infos(&self) -> Vec<ToolInfo> {
        let infos = self.registry.tool_infos().into_iter().map(|info| {
            let source = self.source_of(&info.name);
            ToolInfo {
                name: info.name,
                description: info.description,
                parameters: info.parameters,
                prompt_guidelines: info.prompt_guidelines,
                exposure: info.exposure,
                namespace: info.namespace,
                annotations: info.annotations,
                reaches_tools: info.reaches_tools,
                source: source.source,
                source_path: source.path,
            }
        });
        infos.collect()
    }
}

/// Built-in tools for `cwd` with the SDK options, or `images.autoResize`, `shellPath`, and
/// `shellCommandPrefix` from the settings. `read` fits images to the model of each call, and the
/// shell tools read the `PI_*` variables from `session` unless the options supply them.
pub(crate) fn builtin_tools(
    cwd: &Path,
    settings: &SettingsManager,
    custom: Option<&ToolsOptions>,
    session: Weak<AgentSession>,
) -> Vec<SharedTool> {
    let mut options = custom.cloned().unwrap_or_else(|| ToolsOptions::from_settings(settings));
    let environment = session_environment_fn(session);
    options.bash.session_environment.get_or_insert_with(|| environment.clone());
    options.powershell.session_environment.get_or_insert(environment);
    tools::create_tools_with_options(ALL_TOOLS, cwd, &options).unwrap_or_default()
}

/// The `PI_*` variables of the session, read when each command starts (upstream reads them
/// from the tool call's context).
fn session_environment_fn(session: Weak<AgentSession>) -> SessionEnvironmentFn {
    Arc::new(move || session.upgrade().map(|session| session.environment_variables()).unwrap_or_default())
}

impl AgentSession {
    /// Session id and file, provider, model, and reasoning level as bash `PI_*` variables.
    pub fn environment_variables(&self) -> BTreeMap<String, String> {
        let mut variables = BTreeMap::new();
        variables.insert("PI_SESSION_ID".to_string(), self.session_id());
        if let Some(file) = self.session_file() {
            variables.insert("PI_SESSION_FILE".to_string(), file.display().to_string());
        }
        if let Some(model) = self.model() {
            variables.insert("PI_PROVIDER".to_string(), model.provider);
            variables.insert("PI_MODEL".to_string(), model.id);
        }
        variables.insert("PI_REASONING_LEVEL".to_string(), self.thinking_level().as_str().to_string());
        variables
    }

    /// The built-in tools with this session's tool options.
    pub(crate) fn builtin_tools(&self) -> Vec<SharedTool> {
        let custom = super::lock(&self.tools).tool_options.clone();
        builtin_tools(self.cwd(), self.settings(), custom.as_ref(), self.weak.clone())
    }
}

/// Base prompt options from the loaded resources; the registry fills in the tool fields.
pub(crate) fn prompt_options(cwd: &Path, resources: &LoadedResources) -> BuildSystemPromptOptions {
    BuildSystemPromptOptions::new(cwd).with_resources(resources)
}

/// `defaultTools` (or the defaults) with the `--tools` `+name`/`-name` modifiers applied.
pub(crate) fn default_tool_names(settings: &SettingsManager, modifiers: &[String]) -> Vec<String> {
    let defaults = settings
        .default_tools()
        .unwrap_or_else(|| crate::settings::tools::DEFAULT_TOOL_NAMES.iter().map(|name| name.to_string()).collect());
    apply_tool_modifiers(&defaults, modifiers)
}
