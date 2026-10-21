//! The `pi.agent` document, agent changes, and agent resolution (upstream `harness/agent.ts`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};

use super::define::{Extension, ExtensionHooks, PromptSection, Wrap};
use super::registry::RegistrySnapshot;
use super::settings::Settings;
use super::tool_registration::{ToolCaller, ToolRegistration};
use super::types::{Agent, AgentChange, AgentState, Change, ExtensionSelection, ModelToolFilter, ToolFilter};
use crate::documents::{DocSpec, DocToken, RewindableConversation, define_doc};
use crate::errors::{DurableError, DurableResult};
use crate::ids::ConversationId;
use crate::session::Tx;
use crate::types::{ConversationRecord, DocumentFork};

/// The reserved section key of the agent's `instructions`.
pub const INSTRUCTIONS_KEY: &str = "instructions";

pub type AgentDocToken = DocToken<AgentState, RewindableConversation>;

/// Built-in agent document; rewindable so forks start from the agent at their fork entry.
pub static AGENT_DOC: LazyLock<AgentDocToken> = LazyLock::new(|| {
    let spec =
        DocSpec::new("pi.agent", 1, RewindableConversation(DocumentFork::AsOf), AgentState::default, |_, _, _| {
            Ok(true)
        });
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.agent is a valid definition"))
});

/// Apply one change to `pi.agent`: a set field replaces the stored one, a cleared field is removed.
pub async fn configure(tx: &Tx, conversation_id: ConversationId, change: &AgentChange) -> DurableResult<()> {
    let state = tx.doc(&AGENT_DOC, conversation_id).await?;
    state.edit(|state| apply_change(state, change))
}

/// `addTools` of a tool round, applied to `tools` and `model_tools`: a list gets each missing name appended, and
/// `{ remove }` loses the names. An unset `tools` already enables every tool. An unset or edit `model_tools` gets
/// each name appended to its `add`, which loads deferred tools; by name, so a tool registered later is offered then.
pub async fn add_tools(tx: &Tx, conversation_id: ConversationId, added: &[String]) -> DurableResult<()> {
    let state = tx.doc(&AGENT_DOC, conversation_id).await?;
    state.edit(|state| {
        match &mut state.tools {
            None => {}
            Some(ToolFilter::List(names)) => append(names, added),
            Some(ToolFilter::Remove { remove }) => remove.retain(|name| !added.contains(name)),
        }
        let (remove, mut add) = match state.model_tools.take() {
            Some(ModelToolFilter::List(mut names)) => {
                append(&mut names, added);
                state.model_tools = Some(ModelToolFilter::List(names));
                return;
            }
            Some(ModelToolFilter::Edit { remove, add }) => (remove.unwrap_or_default(), add.unwrap_or_default()),
            None => (Vec::new(), Vec::new()),
        };
        let remove: Vec<String> = remove.into_iter().filter(|name| !added.contains(name)).collect();
        append(&mut add, added);
        let remove = (!remove.is_empty()).then_some(remove);
        state.model_tools = Some(ModelToolFilter::Edit { remove, add: Some(add) });
    })
}

/// Append each of `added` that `names` lacks.
fn append(names: &mut Vec<String>, added: &[String]) {
    for name in added {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
}

fn set<T: Clone>(slot: &mut Option<T>, change: &Change<T>) {
    match change {
        Change::Keep => {}
        Change::Clear => *slot = None,
        Change::Set(value) => *slot = Some(value.clone()),
    }
}

fn apply_change(state: &mut AgentState, change: &AgentChange) {
    set(&mut state.model, &change.model);
    set(&mut state.thinking_level, &change.thinking_level);
    set(&mut state.extensions, &change.extensions);
    set(&mut state.tools, &change.tools);
    set(&mut state.model_tools, &change.model_tools);
    set(&mut state.instructions, &change.instructions);
    set(&mut state.cwd, &change.cwd);
}

/// Built-in part of every commit that creates or forks a conversation, for `pi.agent`: a fork keeps its as-of
/// copy, a new task-owned conversation copies its owner's stored agent, and a new ownerless one starts empty.
pub async fn create_agent(tx: &Tx, conversation: &ConversationRecord) -> DurableResult<()> {
    if conversation.parent.is_some() {
        return Ok(());
    }
    let agent = tx.doc(&AGENT_DOC, conversation.id).await?;
    let Some(owner) = conversation.owner else { return Ok(()) };
    let owner = tx.doc(&AGENT_DOC, owner.conversation_id).await?.get()?;
    agent.set(&owner)
}

/// Handlers of the selected extensions' hooks for a task name, in extension order.
pub fn agent_hooks(agent: &Agent, task_name: &str) -> Vec<ExtensionHooks<dyn std::any::Any + Send + Sync>> {
    let extensions = agent.extensions.iter();
    extensions
        .flat_map(|e| {
            let hooks = e.hooks.iter().filter(|hook| hook.task == task_name);
            hooks.map(|hook| ExtensionHooks { extension: e.name.clone(), handlers: hook.handlers.clone() })
        })
        .collect()
}

/// The extension whose code a section of `agent` runs: the last selected one that provides or wraps it.
pub fn section_extension(agent: &Agent, key: &str) -> Option<String> {
    let runs = |e: &&Arc<Extension>| {
        e.sections.iter().any(|section| section.key == key)
            || e.wraps.iter().any(|wrap| matches!(wrap, Wrap::Section { section, .. } if section == key))
    };
    agent.extensions.iter().rfind(runs).map(|e| e.name.clone())
}

/// The extension whose code a tool of `agent` runs: the last selected one that provides or wraps it.
pub fn tool_extension(agent: &Agent, name: &str) -> Option<String> {
    let runs = |e: &&Arc<Extension>| {
        e.tools.iter().any(|tool| tool.name() == name)
            || e.wraps.iter().any(|wrap| matches!(wrap, Wrap::Tool { tool, .. } if tool == name))
    };
    agent.extensions.iter().rfind(runs).map(|e| e.name.clone())
}

/// Resolve an agent from its stored state, a registry snapshot, and resolved settings. A wrapper that fails or
/// renames drops its target and is reported.
pub fn resolve_agent(
    state: Option<&AgentState>,
    snapshot: &RegistrySnapshot,
    settings: &Settings,
    // Gets a wrapper's failure with the name of the wrapper's extension.
    report: &(dyn Fn(&DurableError, &str) + Sync),
) -> Agent {
    let extensions = select_extensions(state.and_then(|s| s.extensions.as_ref()), snapshot, settings);
    let mut composed: Vec<(String, Arc<ToolRegistration>)> = Vec::new();
    let mut sections: Vec<(String, Arc<PromptSection>)> = Vec::new();
    for extension in &extensions {
        for tool in &extension.tools {
            put(&mut composed, tool.name().to_owned(), tool.clone());
        }
        for section in &extension.sections {
            put(&mut sections, section.key.clone(), section.clone());
        }
    }
    for extension in &extensions {
        let report = |error: &DurableError| report(error, &extension.name);
        for wrap in &extension.wraps {
            match wrap {
                Wrap::Tool { tool, wrap } => apply_wrap(&mut composed, tool, |t| wrap(t), |t| t.name(), &report),
                Wrap::Section { section, wrap } => {
                    apply_wrap(&mut sections, section, |s| wrap(s), |s| s.key.as_str(), &report)
                }
            }
        }
    }
    let composed: Vec<_> = composed.into_iter().map(|(_, tool)| tool).collect();
    let enabled = filter_tools(composed, state.and_then(|s| s.tools.as_ref()));
    let for_model: Vec<_> = enabled.iter().filter(|t| t.callable_by(ToolCaller::Model)).cloned().collect();
    let tools = offered_tools(&for_model, state.and_then(|s| s.model_tools.as_ref()));
    let offered = |tool: &Arc<ToolRegistration>| tools.iter().any(|t| t.name() == tool.name());
    let deferred = for_model.iter().filter(|t| t.deferred && !offered(t)).cloned().collect();
    let callable = enabled.into_iter().filter(|t| t.callable_by(ToolCaller::Tools)).collect();
    let instructions = state.and_then(|s| s.instructions.clone());
    let mut agent_sections: Vec<_> = sections.into_iter().map(|(_, section)| section).collect();
    if let Some(text) = &instructions {
        agent_sections.push(super::define::static_section(INSTRUCTIONS_KEY, text.clone()));
    }
    Agent {
        model: state.and_then(|s| s.model.clone()),
        thinking_level: state.and_then(|s| s.thinking_level).unwrap_or_default(),
        extensions,
        tools,
        callable,
        deferred,
        sections: agent_sections,
        instructions,
        cwd: state.and_then(|s| s.cwd.clone()),
    }
}

/// Insert or replace by key, keeping the first insertion's position (JavaScript `Map.set`).
fn put<T>(items: &mut Vec<(String, T)>, key: String, value: T) {
    match items.iter_mut().find(|(existing, _)| *existing == key) {
        Some(slot) => slot.1 = value,
        None => items.push((key, value)),
    }
}

/// The tools offered to the model, as `AgentState::model_tools` describes.
fn offered_tools(tools: &[Arc<ToolRegistration>], filter: Option<&ModelToolFilter>) -> Vec<Arc<ToolRegistration>> {
    let (remove, add) = match filter {
        Some(ModelToolFilter::List(names)) => {
            return filter_tools(tools.to_vec(), Some(&ToolFilter::List(names.clone())));
        }
        Some(ModelToolFilter::Edit { remove, add }) => (remove.as_deref(), add.as_deref()),
        None => (None, None),
    };
    let removed = |name: &str| remove.unwrap_or_default().iter().any(|n| n == name);
    let kept = tools.iter().filter(|t| !t.deferred && !removed(t.name()));
    // Loaded deferred tools follow in load order, so loading one appends to the declared tools.
    let mut seen = HashSet::new();
    let loaded = add
        .unwrap_or_default()
        .iter()
        .filter(|name| seen.insert(name.as_str()))
        .filter_map(|name| tools.iter().find(|t| t.name() == name && t.deferred && !removed(name)));
    kept.chain(loaded).cloned().collect()
}

/// `tools` through a stored filter: a list selects exactly its names, in its order; `{ remove }` drops names.
fn filter_tools(tools: Vec<Arc<ToolRegistration>>, filter: Option<&ToolFilter>) -> Vec<Arc<ToolRegistration>> {
    match filter {
        None => tools,
        Some(ToolFilter::List(names)) => {
            let by_name: HashMap<&str, &Arc<ToolRegistration>> = tools.iter().map(|t| (t.name(), t)).collect();
            let mut seen = HashSet::new();
            names
                .iter()
                .filter(|name| seen.insert(name.as_str()))
                .filter_map(|n| by_name.get(n.as_str()).map(|t| (*t).clone()))
                .collect()
        }
        Some(ToolFilter::Remove { remove }) => {
            tools.into_iter().filter(|t| !remove.iter().any(|n| n == t.name())).collect()
        }
    }
}

/// Selected installed extensions: the stored list, or the default selection edited by `{ add, remove }`.
fn select_extensions(
    stored: Option<&ExtensionSelection>,
    snapshot: &RegistrySnapshot,
    settings: &Settings,
) -> Vec<Arc<Extension>> {
    let selected: Vec<String> = match stored {
        Some(ExtensionSelection::List(names)) => names.clone(),
        edit => {
            let base: Vec<String> = match &settings.extensions {
                Some(extensions) => extensions.iter().map(|e| e.name.clone()).collect(),
                None => snapshot.installed().iter().map(|e| e.name.clone()).collect(),
            };
            let (add, remove) = match edit {
                Some(ExtensionSelection::Edit { add, remove }) => {
                    (add.clone().unwrap_or_default(), remove.clone().unwrap_or_default())
                }
                _ => (Vec::new(), Vec::new()),
            };
            base.into_iter().chain(add).filter(|name| !remove.contains(name)).collect()
        }
    };
    let mut seen = HashSet::new();
    selected.iter().filter(|name| seen.insert(name.as_str())).filter_map(|name| snapshot.extension(name)).collect()
}

fn apply_wrap<T: Clone>(
    items: &mut Vec<(String, T)>,
    target: &str,
    wrap: impl Fn(T) -> DurableResult<T>,
    name_of: impl Fn(&T) -> &str,
    report: &(dyn Fn(&DurableError) + Sync),
) {
    let Some(index) = items.iter().position(|(key, _)| key == target) else { return };
    let wrapped = wrap(items[index].1.clone()).and_then(|wrapped| {
        if name_of(&wrapped) == target {
            Ok(wrapped)
        } else {
            Err(DurableError::error(format!("Wrapper renamed {target} to {}", name_of(&wrapped))))
        }
    });
    match wrapped {
        Ok(wrapped) => items[index].1 = wrapped,
        Err(error) => {
            items.remove(index);
            report(&error);
        }
    }
}
