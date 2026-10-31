//! The session's tool API (upstream `getActiveToolNames`, `getCallableToolNames`, `getAllTools`,
//! `setActiveToolsByName`, `_preparePromptAndToolLoadout`, `_restoreToolsFromTranscript`) and the
//! registration entry points extensions and the built-in extensions use.

use std::collections::HashSet;

use pi_agent::{SharedTool, ToolRegistry};
use pi_ai::transcript::{current_system_message, current_tools, tool_state_changes};
use pi_ai::{Message, SystemMessage, UserContent};

use super::tools::{ActiveSelection, ToolInfo, ToolSource, default_tool_names, prompt_options};
use super::{AgentSession, lock};
use crate::settings::tools::apply_tool_modifiers;
use crate::system_prompt::{
    BuildSystemPromptOptions, build_system_prompt_sections, diff_system_prompt_sections, render_system_prompt,
};

impl AgentSession {
    /// Names of the active tools, which are the tools declared to the model. Tools with
    /// `codemode` or `deferred` exposure are callable from other tools without being active.
    pub fn active_tool_names(&self) -> Vec<String> {
        lock(&self.tools).registry.active_names()
    }

    /// Names of the tools other tools can call through `ToolContext::execute_tool`.
    pub fn callable_tool_names(&self) -> Vec<String> {
        lock(&self.tools).registry.callable_names()
    }

    /// Every registered tool with its exposure and source.
    pub fn all_tools(&self) -> Vec<ToolInfo> {
        lock(&self.tools).infos()
    }

    /// A registered tool by name.
    pub fn tool(&self, name: &str) -> Option<SharedTool> {
        lock(&self.tools).registry.get(name).cloned()
    }

    /// Read the tool registry. Do not call back into the session from `read`.
    pub fn with_tool_registry<R>(&self, read: impl FnOnce(&ToolRegistry) -> R) -> R {
        read(&lock(&self.tools).registry)
    }

    /// Set the active tools by name; unknown and hidden names are ignored. The prompt and tool
    /// declarations change before the next request.
    pub fn set_active_tools_by_name(&self, names: &[String]) {
        {
            let mut tools = lock(&self.tools);
            // A loadout that deactivates a tool replaces the restored one, whose pending tools are
            // dropped. One that only adds tools, like a `tool_search` load, keeps them.
            if tools.set_active(names) {
                tools.pending.clear();
            }
        }
        self.sync_agent_tools();
    }

    /// Register tools, replacing registered tools of the same names. A new tool whose exposure
    /// is `direct` or `model-only` becomes active unless it is inactive by default.
    pub fn register_tools(&self, tools: Vec<SharedTool>, source: ToolSource) {
        {
            let mut state = lock(&self.tools);
            state.register(tools, &source);
            state.refresh(ActiveSelection::default());
        }
        self.sync_agent_tools();
    }

    pub fn register_tool(&self, tool: SharedTool, source: ToolSource) {
        self.register_tools(vec![tool], source);
    }

    /// Remove a tool registered with [`Self::register_tools`]. Built-in tools stay.
    pub fn unregister_tool(&self, name: &str) -> bool {
        let removed = {
            let mut state = lock(&self.tools);
            let removed = state.unregister(name);
            if removed {
                state.refresh(ActiveSelection::default());
            }
            removed
        };
        if removed {
            self.sync_agent_tools();
        }
        removed
    }

    /// Replace every tool registered from `source.path`, as an extension re-registering its tools.
    pub(crate) fn replace_source_tools(&self, tools: Vec<SharedTool>, source: &ToolSource) {
        {
            let mut state = lock(&self.tools);
            state.replace_source(&source.path, tools, source);
            state.refresh(ActiveSelection::default());
        }
        self.sync_agent_tools();
    }

    pub(crate) fn refresh_tools(&self, selection: ActiveSelection) {
        lock(&self.tools).refresh(selection);
        self.sync_agent_tools();
    }

    /// Give the agent the active tools with their loadout descriptions.
    pub(crate) fn sync_agent_tools(&self) {
        let active = lock(&self.tools).registry.active_tools().to_vec();
        self.agent.update_state(|state| state.tools = active);
    }

    /// Declarations `prepare_loadout` hooks hide from requests.
    pub(crate) fn hidden_declarations(&self) -> Vec<String> {
        lock(&self.tools).registry.hidden_declarations().to_vec()
    }

    /// Rebuild the base prompt from the resource loader (after `/reload` or resource changes).
    pub(crate) fn rebuild_system_prompt(&self) {
        let resources = self.services.resources().resources().clone();
        lock(&self.tools).base_prompt = prompt_options(self.cwd(), &resources);
    }

    pub(crate) fn current_default_tools(&self) -> Vec<String> {
        let (uses, modifiers) = {
            let tools = lock(&self.tools);
            (tools.uses_default_tools, tools.default_tool_modifiers.clone())
        };
        if uses { default_tool_names(self.settings(), &modifiers) } else { Vec::new() }
    }

    /// Restore the loadout the branch records, if it records one, with `+name`/`-name` `modifiers`
    /// applied. Extension tools the branch never added or removed were registered after it recorded
    /// its loadout, so they are activated like newly registered tools.
    pub(crate) fn restore_tools_from_transcript(&self, modifiers: &[String]) {
        // Every system message on the branch, including those before a compaction.
        let systems: Vec<Message> = {
            let session = self.session_manager();
            let branch = session.get_branch(None);
            branch
                .into_iter()
                .filter_map(|entry| entry.message())
                .filter(|message| message.role() == "system")
                .cloned()
                .collect()
        };
        if systems.is_empty() {
            lock(&self.tools).pending.clear();
            return;
        }
        let mentioned: HashSet<String> = systems
            .iter()
            .filter_map(|message| match message {
                Message::System(system) => Some(system),
                _ => None,
            })
            .flat_map(|system| {
                let added = system.tools_added.iter().flatten().map(|tool| tool.name.clone());
                added.chain(system.tools_removed.iter().flatten().map(|tool| tool.name.clone()))
            })
            .collect();
        let mut declared: Vec<String> = current_tools(&systems).into_iter().map(|tool| tool.name).collect();
        {
            // Built-in tools follow `defaultTools` instead.
            let tools = lock(&self.tools);
            let new_tools = tools.registry.names().into_iter().filter(|name| {
                !mentioned.contains(name)
                    && !tools.is_builtin(name)
                    && tools.registry.is_activated_on_registration(name)
            });
            declared.extend(new_tools);
        }
        let names = apply_tool_modifiers(&declared, modifiers);
        {
            // Recorded tools that are not registered yet, such as MCP tools of servers still
            // connecting, become active when they register. Excluded tools never register.
            let mut tools = lock(&self.tools);
            tools.pending = names.iter().filter(|name| tools.is_allowed(name)).cloned().collect();
            tools.set_active(&names);
        }
        self.sync_agent_tools();
    }

    /// The prompt options for the next request: the run's options, else the base ones, with the
    /// tool fields of the current registry.
    pub(crate) fn effective_prompt_options(&self) -> BuildSystemPromptOptions {
        let run = lock(&self.state).run_prompt.clone();
        let tools = lock(&self.tools);
        let mut options = run.unwrap_or_else(|| tools.base_prompt.clone());
        tools.apply_to_prompt(&mut options);
        options
    }

    /// The current system prompt text, including changes not yet sent to the model.
    pub fn system_prompt(&self) -> String {
        render_system_prompt(&self.effective_prompt_options()).unwrap_or_default()
    }

    /// Apply the loadout of `options` and return a system message patching the prompt sections and
    /// tool declarations the model has (replayed from `messages`), or `None` when nothing changed.
    pub(crate) fn prepare_prompt_and_tool_loadout(
        &self,
        options: &BuildSystemPromptOptions,
        messages: &[Message],
    ) -> Option<SystemMessage> {
        let mut options = options.clone();
        let declared: Vec<pi_ai::Tool> = {
            let mut tools = lock(&self.tools);
            tools.set_active(&options.selected_tools);
            tools.apply_to_prompt(&mut options);
            tools.registry.active_tools().iter().map(|tool| tool.declaration()).collect()
        };
        self.sync_agent_tools();
        let current = current_system_message(messages);
        let previous = current.and_then(|system| system.sections).unwrap_or_default();
        let wanted = build_system_prompt_sections(&options).unwrap_or_default();
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
