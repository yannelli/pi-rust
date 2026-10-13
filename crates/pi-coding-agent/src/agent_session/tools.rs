//! The tool registry, active tool loadout, and system prompt sections (upstream
//! `_refreshToolRegistry`, `_rebuildSystemPrompt`, `_preparePromptAndToolLoadout`).

use std::path::Path;

use pi_agent::SharedTool;
use pi_ai::transcript::{current_system_message, current_tools, tool_state_changes};
use pi_ai::{Message, SystemMessage, UserContent};
use serde_json::Value;

use super::{AgentSession, lock};
use crate::resources::LoadedResources;
use crate::settings::SettingsManager;
use crate::settings::tools::{apply_tool_modifiers, matches_any_tool};
use crate::system_prompt::{
    BuildSystemPromptOptions, build_system_prompt_sections, diff_system_prompt_sections, render_system_prompt,
};
use crate::tools::{self, ALL_TOOLS};

/// A registered tool as `getAllTools` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolInfo {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub prompt_guidelines: Vec<String>,
    /// `builtin` or `sdk`.
    pub source: String,
}

pub(crate) struct ToolState {
    /// Registered tools in order: built-ins, then SDK tools.
    pub registry: Vec<SharedTool>,
    pub custom: Vec<SharedTool>,
    pub active: Vec<String>,
    /// `--tools` names or patterns; `None` allows every tool.
    pub allowed: Option<Vec<String>>,
    pub excluded: Vec<String>,
    pub uses_default_tools: bool,
    pub default_tool_modifiers: Vec<String>,
    /// Prompt options for the next run; `selected_tools` follows `active`.
    pub base_prompt: BuildSystemPromptOptions,
}

impl ToolState {
    pub fn is_allowed(&self, name: &str) -> bool {
        !matches_any_tool(&self.excluded, name)
            && self.allowed.as_ref().is_none_or(|allowed| matches_any_tool(allowed, name))
    }

    fn get(&self, name: &str) -> Option<&SharedTool> {
        self.registry.iter().find(|tool| tool.name() == name)
    }

    fn selected(&self) -> Vec<SharedTool> {
        self.active.iter().filter_map(|name| self.get(name).cloned()).collect()
    }
}

/// Built-in tools for `cwd` with `images.autoResize`, `shellPath`, and `shellCommandPrefix`.
/// `read` fits images to the model of each call.
pub(crate) fn builtin_tools(cwd: &Path, settings: &SettingsManager) -> Vec<SharedTool> {
    let options = tools::ToolsOptions::from_settings(settings);
    tools::create_tools_with_options(ALL_TOOLS, cwd, &options).unwrap_or_default()
}

/// Base prompt options from the loaded resources, with snippets and guidelines of every
/// built-in tool and `active` as the selected tools.
pub(crate) fn prompt_options(cwd: &Path, resources: &LoadedResources, active: &[String]) -> BuildSystemPromptOptions {
    let mut options = BuildSystemPromptOptions::new(cwd).with_resources(resources);
    options.selected_tools = ALL_TOOLS.iter().map(|name| name.to_string()).collect();
    let mut options = options.with_builtin_tool_info();
    options.selected_tools = active.to_vec();
    options
}

/// `defaultTools` (or the defaults) with the `--tools` `+name`/`-name` modifiers applied.
pub(crate) fn default_tool_names(settings: &SettingsManager, modifiers: &[String]) -> Vec<String> {
    let defaults = settings
        .default_tools()
        .unwrap_or_else(|| crate::settings::tools::DEFAULT_TOOL_NAMES.iter().map(|name| name.to_string()).collect());
    apply_tool_modifiers(&defaults, modifiers)
}

impl AgentSession {
    /// Names of the tools declared to the model.
    pub fn active_tool_names(&self) -> Vec<String> {
        lock(&self.tools).active.clone()
    }

    /// Every registered tool with its source.
    pub fn all_tools(&self) -> Vec<ToolInfo> {
        let tools = lock(&self.tools);
        tools
            .registry
            .iter()
            .map(|tool| {
                let custom = tools.custom.iter().any(|custom| custom.name() == tool.name());
                ToolInfo {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters(),
                    prompt_guidelines: tools::tool_info(tool.name())
                        .filter(|_| !custom)
                        .map(|info| info.guidelines.iter().map(|rule| rule.to_string()).collect())
                        .unwrap_or_default(),
                    source: if custom { "sdk" } else { "builtin" }.to_string(),
                }
            })
            .collect()
    }

    /// Set the active tools by name; unknown names are ignored. The prompt and tool
    /// declarations change before the next request.
    pub fn set_active_tools_by_name(&self, names: &[String]) {
        let selected = {
            let mut tools = lock(&self.tools);
            let mut active: Vec<String> = Vec::new();
            for name in names {
                if tools.get(name).is_some() && !active.contains(name) {
                    active.push(name.clone());
                }
            }
            tools.base_prompt.selected_tools = active.clone();
            tools.active = active;
            tools.selected()
        };
        self.agent.update_state(|state| state.tools = selected);
    }

    /// Rebuild the base prompt from the resource loader (after `/reload` or resource changes).
    pub(crate) fn rebuild_system_prompt(&self) {
        let resources = self.services.resources().resources().clone();
        let mut tools = lock(&self.tools);
        let active = tools.active.clone();
        tools.base_prompt = prompt_options(self.cwd(), &resources, &active);
    }

    /// Rebuild the registry from the current settings.
    pub(crate) fn refresh_builtin_tools(&self) {
        let builtins = builtin_tools(self.cwd(), self.settings());
        let selected = {
            let mut tools = lock(&self.tools);
            let mut registry: Vec<SharedTool> =
                builtins.into_iter().filter(|tool| tools.is_allowed(tool.name())).collect();
            for tool in tools.custom.clone().into_iter().filter(|tool| tools.is_allowed(tool.name())) {
                registry.retain(|existing| existing.name() != tool.name());
                registry.push(tool);
            }
            tools.registry = registry;
            let registered: Vec<String> = tools.registry.iter().map(|tool| tool.name().to_string()).collect();
            tools.active.retain(|name| registered.contains(name));
            tools.selected()
        };
        self.agent.update_state(|state| state.tools = selected);
    }

    /// After `/reload`, activate tools that `defaultTools` newly lists.
    pub(crate) fn activate_new_default_tools(&self, previous_defaults: &[String]) {
        let mut next = self.active_tool_names();
        for name in self.current_default_tools() {
            if !previous_defaults.contains(&name) && !next.contains(&name) {
                next.push(name);
            }
        }
        self.set_active_tools_by_name(&next);
    }

    pub(crate) fn current_default_tools(&self) -> Vec<String> {
        let (uses, modifiers) = {
            let tools = lock(&self.tools);
            (tools.uses_default_tools, tools.default_tool_modifiers.clone())
        };
        if uses { default_tool_names(self.settings(), &modifiers) } else { Vec::new() }
    }

    /// The prompt options for the next request: the run's options, else the base ones.
    pub(crate) fn effective_prompt_options(&self) -> BuildSystemPromptOptions {
        let run = lock(&self.state).run_prompt.clone();
        let tools = lock(&self.tools);
        let mut options = run.unwrap_or_else(|| tools.base_prompt.clone());
        options.selected_tools = tools.active.clone();
        options
    }

    /// The current system prompt text, including changes not yet sent to the model.
    pub fn system_prompt(&self) -> String {
        render_system_prompt(&self.effective_prompt_options()).unwrap_or_default()
    }

    /// A system message patching the prompt sections and tool declarations the model has
    /// (replayed from `messages`) to `options`, or `None` when nothing changed.
    pub(crate) fn prepare_prompt_and_tool_loadout(
        &self,
        options: &BuildSystemPromptOptions,
        messages: &[Message],
    ) -> Option<SystemMessage> {
        let declared: Vec<pi_ai::Tool> = {
            let tools = lock(&self.tools);
            tools.selected().iter().map(|tool| tool.declaration()).collect()
        };
        let current = current_system_message(messages);
        let previous = current.and_then(|system| system.sections).unwrap_or_default();
        let wanted = build_system_prompt_sections(options).unwrap_or_default();
        let sections = diff_system_prompt_sections(&previous, &wanted);
        let (added, removed) = tool_state_changes(&current_tools(messages), &declared);
        if sections.is_none() && added.is_empty() && removed.is_empty() {
            return None;
        }
        Some(SystemMessage {
            content: UserContent::Text(String::new()),
            sections,
            tools_added: (!added.is_empty()).then_some(added),
            tools_removed: (!removed.is_empty()).then_some(removed),
            timestamp: pi_ai::now_ms(),
        })
    }
}
