//! The tool registry: registered tools, the active set declared to the model, the callable set
//! other tools reach, and `prepare_loadout` changes. Port of the registry logic in upstream
//! `agent-session.ts` (`_refreshToolRegistry`, `_applyToolLoadout`, `_getCallableTools`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::described::with_description;
use crate::tool_model::{
    ToolAnnotations, ToolExposure, ToolLoadout, ToolNamespace, normalize_prompt_guidelines, normalize_prompt_snippet,
};
use crate::types::SharedTool;

/// Extra rule for which tools may be active, such as a `--tools` allowlist.
pub type ActivationFilter = Arc<dyn Fn(&str, ToolExposure) -> bool + Send + Sync>;

/// A registered tool as listings see it (upstream `getAllTools`).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prompt_guidelines: Vec<String>,
    pub exposure: ToolExposure,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<ToolNamespace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    /// The tool has `reaches_tools`. Upstream's `reachesTools` is a function, so listings that
    /// serialize leave it out.
    #[serde(skip)]
    pub reaches_tools: bool,
}

/// Options for [`ToolRegistry::refresh`].
#[derive(Clone, Debug, Default)]
pub struct RefreshOptions {
    /// The active set to apply. Unset keeps the tools that were active and activates tools
    /// that registration activates for the first time.
    pub active_tool_names: Option<Vec<String>>,
    /// Names activated in addition, such as restored tools that registered late.
    pub extra_active: Vec<String>,
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: Vec<SharedTool>,
    /// Declared to the model, with descriptions from `prepare_loadout`.
    active: Vec<SharedTool>,
    hidden_declarations: Vec<String>,
    activation_filter: Option<ActivationFilter>,
}

impl ToolRegistry {
    /// A registry with `tools` registered and activated as registration activates them.
    pub fn new(tools: impl IntoIterator<Item = SharedTool>) -> Self {
        let mut registry = Self::default();
        registry.refresh(tools.into_iter().collect(), RefreshOptions::default());
        registry
    }

    /// Set the extra activation rule and re-apply the active set.
    pub fn set_activation_filter(&mut self, filter: Option<ActivationFilter>) {
        self.activation_filter = filter;
        self.set_active_tools(&self.active_names());
    }

    pub fn get(&self, name: &str) -> Option<&SharedTool> {
        self.tools.iter().find(|tool| tool.name() == name)
    }

    /// Every registered tool, in registration order.
    pub fn tools(&self) -> &[SharedTool] {
        &self.tools
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.name().to_string()).collect()
    }

    /// A tool's exposure; unknown tools are `Direct`.
    pub fn exposure(&self, name: &str) -> ToolExposure {
        self.get(name).map(|tool| tool.exposure()).unwrap_or_default()
    }

    /// Whether activating the tool declares it to the model.
    pub fn is_declarable(&self, name: &str) -> bool {
        self.exposure(name).is_declarable()
    }

    /// Whether registering the tool activates it.
    pub fn is_activated_on_registration(&self, name: &str) -> bool {
        self.get(name).is_some_and(|tool| tool.exposure().is_declarable() && tool.default_active())
    }

    /// Whether the tool may be active: registered, not hidden, and allowed by the filter.
    pub fn is_activatable(&self, name: &str) -> bool {
        let exposure = self.exposure(name);
        self.get(name).is_some()
            && exposure != ToolExposure::Hidden
            && self.activation_filter.as_ref().is_none_or(|filter| filter(name, exposure))
    }

    /// Register a tool, replacing one with the same name. A new tool activated on registration
    /// becomes active, as does one whose exposure changed to a declarable one.
    pub fn register(&mut self, tool: SharedTool) {
        let mut tools = self.tools.clone();
        match tools.iter_mut().find(|existing| existing.name() == tool.name()) {
            Some(existing) => *existing = tool,
            None => tools.push(tool),
        }
        self.refresh(tools, RefreshOptions::default());
    }

    pub fn unregister(&mut self, name: &str) -> Option<SharedTool> {
        let removed = self.get(name).cloned()?;
        let tools = self.tools.iter().filter(|tool| tool.name() != name).cloned().collect();
        self.refresh(tools, RefreshOptions::default());
        Some(removed)
    }

    /// Replace the registered tools (later duplicates win, keeping the first position) and
    /// re-apply the active set.
    pub fn refresh(&mut self, tools: Vec<SharedTool>, options: RefreshOptions) {
        let activated_before: HashSet<String> =
            self.names().into_iter().filter(|name| self.is_activated_on_registration(name)).collect();
        let active_before = self.active_names();
        let mut registered: Vec<SharedTool> = Vec::new();
        for tool in tools {
            match registered.iter_mut().find(|existing| existing.name() == tool.name()) {
                Some(existing) => *existing = tool,
                None => registered.push(tool),
            }
        }
        self.tools = registered;
        let explicit = options.active_tool_names.is_some();
        let mut next = options.active_tool_names.unwrap_or(active_before);
        if !explicit {
            for name in self.names() {
                if !activated_before.contains(&name) && self.is_activated_on_registration(&name) {
                    next.push(name);
                }
            }
        }
        next.extend(options.extra_active);
        self.set_active_tools(&next);
    }

    /// Activated-on-registration tools, in order: the default active set.
    pub fn default_active_names(&self) -> Vec<String> {
        self.names().into_iter().filter(|name| self.is_activated_on_registration(name)).collect()
    }

    /// The active tools, declared to the model, with loadout descriptions applied.
    pub fn active_tools(&self) -> &[SharedTool] {
        &self.active
    }

    pub fn active_names(&self) -> Vec<String> {
        self.active.iter().map(|tool| tool.name().to_string()).collect()
    }

    /// Declared tools whose declarations requests leave out (`prepare_loadout`).
    pub fn hidden_declarations(&self) -> &[String] {
        &self.hidden_declarations
    }

    /// Tools other tools can call: the active `Direct` tools and every `Codemode` or `Deferred` tool.
    pub fn callable_tools(&self) -> Vec<SharedTool> {
        let active: HashSet<String> = self.active_names().into_iter().collect();
        self.callable_for(&active)
    }

    pub fn callable_names(&self) -> Vec<String> {
        self.callable_tools().iter().map(|tool| tool.name().to_string()).collect()
    }

    fn callable_for(&self, active: &HashSet<String>) -> Vec<SharedTool> {
        let callable = |tool: &&SharedTool| {
            let exposure = tool.exposure();
            exposure.is_callable_while_inactive() || (exposure == ToolExposure::Direct && active.contains(tool.name()))
        };
        self.tools.iter().filter(callable).cloned().collect()
    }

    /// Normalized prompt guidelines of every registered tool that has some.
    pub fn prompt_guidelines(&self) -> HashMap<String, Vec<String>> {
        let entries = self
            .tools
            .iter()
            .map(|tool| (tool.name().to_string(), normalize_prompt_guidelines(&tool.prompt_guidelines())));
        entries.filter(|(_, guidelines)| !guidelines.is_empty()).collect()
    }

    /// One-line prompt snippets of every registered tool that has one.
    pub fn prompt_snippets(&self) -> BTreeMap<String, String> {
        let entries = self
            .tools
            .iter()
            .filter_map(|tool| Some((tool.name().to_string(), normalize_prompt_snippet(tool.prompt_snippet())?)));
        entries.collect()
    }

    pub fn tool_infos(&self) -> Vec<ToolInfo> {
        let info = |tool: &SharedTool| ToolInfo {
            name: tool.name().into(),
            description: tool.description().into(),
            parameters: tool.parameters(),
            prompt_guidelines: tool.prompt_guidelines(),
            exposure: tool.exposure(),
            namespace: tool.namespace().cloned(),
            annotations: tool.annotations().cloned(),
            reaches_tools: tool.declares_reaches_tools(),
        };
        self.tools.iter().map(info).collect()
    }

    /// The loadout `prepare_loadout` hooks see for `declared`.
    pub fn loadout(&self, declared: Vec<SharedTool>) -> ToolLoadout {
        let active: HashSet<String> = declared.iter().map(|tool| tool.name().to_string()).collect();
        ToolLoadout {
            callable: self.callable_for(&active),
            registered: self.tools.clone(),
            guidelines: self.prompt_guidelines(),
            declared,
        }
    }

    /// Activate `names` (unknown, hidden, and filtered names are ignored) and return the declared
    /// tools. Active tools' `prepare_loadout` hooks may change descriptions and hide declarations.
    pub fn set_active_tools<S: AsRef<str>>(&mut self, names: &[S]) -> Vec<SharedTool> {
        let mut seen = HashSet::new();
        let tools: Vec<SharedTool> = names
            .iter()
            .map(AsRef::as_ref)
            .filter(|name| seen.insert(name.to_string()) && self.is_activatable(name))
            .filter_map(|name| self.get(name).cloned())
            .collect();
        let loadout = self.loadout(tools.clone());
        let mut descriptions = BTreeMap::new();
        let mut hidden: Vec<String> = Vec::new();
        for changes in tools.iter().filter_map(|tool| tool.prepare_loadout(&loadout)) {
            descriptions.extend(changes.descriptions);
            for name in changes.hidden_declarations {
                if !hidden.contains(&name) {
                    hidden.push(name);
                }
            }
        }
        self.active = tools
            .into_iter()
            .map(|tool| match descriptions.remove(tool.name()) {
                Some(description) => with_description(tool, description),
                None => tool,
            })
            .collect();
        self.hidden_declarations = hidden;
        self.active.clone()
    }
}
