//! The positional system prompt (upstream `harness/prompt.ts`): section rendering and the `pi.system` entries
//! that move the replayed sections and tools to the wanted ones.

use std::collections::HashSet;
use std::sync::Arc;

use pi_ai::transcript::current_tools;
use pi_ai::{JsonObject, Message, SystemMessage, Tool, ToolReference, UserContent};
use pi_chord::Context;
use serde_json::Value;

use super::define::{PromptInput, PromptSection};
use super::types::ContextView;
use crate::entries::SYSTEM_ENTRY;
use crate::errors::{DurableError, DurableResult};
use crate::types::{ContextEdit, EntryDraft};

/// Ordered key and text pairs, as JavaScript maps keep them.
pub type Sections = Vec<(String, String)>;

fn get<'a>(sections: &'a Sections, key: &str) -> Option<&'a String> {
    sections.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Sections in effect after replaying system messages in order: set in place, `null` deletes, re-adding appends.
pub fn replay_sections(messages: &[Message]) -> Sections {
    let mut shown: Sections = Vec::new();
    for message in messages {
        let Message::System(system) = message else { continue };
        for (key, value) in system.sections.iter().flatten() {
            match value {
                Value::String(text) => match shown.iter_mut().find(|(k, _)| k == key) {
                    Some(slot) => slot.1 = text.clone(),
                    None => shown.push((key.clone(), text.clone())),
                },
                _ => shown.retain(|(k, _)| k != key),
            }
        }
    }
    shown
}

/// Render the agent's sections in order. `None` omits a section; tagged text is wrapped as `<key>\n...\n</key>`.
/// A section that fails keeps its shown text and is reported; errors after `context` is cancelled propagate.
pub async fn render_sections(
    sections: &[Arc<PromptSection>],
    input: &PromptInput,
    shown: &Sections,
    report: &(dyn Fn(&DurableError, &PromptSection) + Sync),
    context: &Context,
) -> DurableResult<Sections> {
    let mut wanted = Vec::new();
    for section in sections {
        let text = match (section.render)(input.clone(), context.clone()).await {
            Ok(text) => text,
            Err(error) => {
                if context.is_cancelled() {
                    return Err(error);
                }
                report(&error, section);
                if let Some(kept) = get(shown, &section.key) {
                    wanted.push((section.key.clone(), kept.clone()));
                }
                continue;
            }
        };
        let Some(text) = text else { continue };
        let text = if section.tag == Some(false) { text } else { format!("<{0}>\n{text}\n</{0}>", section.key) };
        match wanted.iter_mut().find(|(k, _)| *k == section.key) {
            Some(slot) => slot.1 = text,
            None => wanted.push((section.key.clone(), text)),
        }
    }
    Ok(wanted)
}

struct ToolChanges {
    removed: Vec<ToolReference>,
    added: Vec<Tool>,
}

/// A tool's declaration as the transcript stores it.
pub fn to_tool_declaration(tool: &Tool) -> Tool {
    tool.clone()
}

/// Plan the `pi.system` entries that make the replayed sections and tools of `view` equal the wanted ones. A head
/// marker without a later `pi.system` entry gets one complete baseline; otherwise a minimal patch.
pub fn plan_system_entries(view: &ContextView, wanted: &Sections, tools: &[Tool], timestamp: f64) -> Vec<EntryDraft> {
    if let Some(head) = &view.head {
        let later = view.entries.iter().any(|entry| SYSTEM_ENTRY.is(Some(entry)) && entry.id > head.id);
        if !later {
            let edits: Vec<ContextEdit> = view
                .entries
                .iter()
                .filter(|entry| SYSTEM_ENTRY.is(Some(entry)))
                .map(|entry| ContextEdit::omit(entry.id))
                .collect();
            let patch = wanted.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
            let changes = ToolChanges { removed: Vec::new(), added: tools.iter().map(to_tool_declaration).collect() };
            let mut baseline = system_entry(Some(patch), Some(changes), timestamp);
            if !edits.is_empty() {
                baseline.edits = Some(edits);
            }
            return vec![baseline];
        }
    }
    let sections = plan_sections(&replay_sections(&view.messages), wanted);
    let changes = plan_tools(&current_tools(&view.messages), tools);
    if changes.removed.is_empty() && changes.added.is_empty() {
        return sections.into_iter().map(|patch| system_entry(Some(patch), None, timestamp)).collect();
    }
    if sections.is_empty() {
        return vec![system_entry(None, Some(changes), timestamp)];
    }
    let count = sections.len();
    let mut changes = Some(changes);
    let mut entries = Vec::new();
    for (index, patch) in sections.into_iter().enumerate() {
        let tools = if index == count - 1 { changes.take() } else { None };
        entries.push(system_entry(Some(patch), tools, timestamp));
    }
    entries
}

/// Tool changes from `offered` to `wanted`; a changed declaration is removed and added again.
fn plan_tools(offered: &[Tool], wanted: &[Tool]) -> ToolChanges {
    let kept: Vec<&Tool> =
        offered.iter().filter(|tool| wanted.iter().any(|next| next.name == tool.name && *next == **tool)).collect();
    let kept_names: HashSet<&str> = kept.iter().map(|tool| tool.name.as_str()).collect();
    let added: Vec<&Tool> = wanted.iter().filter(|tool| !kept_names.contains(tool.name.as_str())).collect();
    let replayed: Vec<&str> = kept.iter().chain(added.iter()).map(|tool| tool.name.as_str()).collect();
    let reordered = replayed.iter().enumerate().any(|(index, name)| wanted.get(index).is_none_or(|t| t.name != *name));
    if reordered {
        return ToolChanges {
            removed: offered.iter().map(|tool| ToolReference { name: tool.name.clone() }).collect(),
            added: wanted.iter().map(to_tool_declaration).collect(),
        };
    }
    ToolChanges {
        removed: offered
            .iter()
            .filter(|tool| !kept_names.contains(tool.name.as_str()))
            .map(|tool| ToolReference { name: tool.name.clone() })
            .collect(),
        added: added.into_iter().map(to_tool_declaration).collect(),
    }
}

type Patch = Vec<(String, Option<String>)>;

/// Section patches: none, the minimal patch, or a remove-all and add-all pair when the order would differ.
fn plan_sections(shown: &Sections, wanted: &Sections) -> Vec<Patch> {
    let has = |sections: &Sections, key: &str| sections.iter().any(|(k, _)| k == key);
    let patched: Vec<&str> = shown
        .iter()
        .filter(|(k, _)| has(wanted, k))
        .chain(wanted.iter().filter(|(k, _)| !has(shown, k)))
        .map(|(k, _)| k.as_str())
        .collect();
    if patched.iter().zip(wanted.iter()).any(|(key, (want, _))| key != want) {
        let removal = shown.iter().map(|(k, _)| (k.clone(), None)).collect();
        let all = wanted.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
        return vec![removal, all];
    }
    let mut patch: Patch = Vec::new();
    for (key, value) in shown {
        let next = get(wanted, key);
        if next != Some(value) {
            patch.push((key.clone(), next.cloned()));
        }
    }
    for (key, value) in wanted {
        if !has(shown, key) {
            patch.push((key.clone(), Some(value.clone())));
        }
    }
    if patch.is_empty() { Vec::new() } else { vec![patch] }
}

fn system_entry(sections: Option<Patch>, tools: Option<ToolChanges>, timestamp: f64) -> EntryDraft {
    let sections = sections.map(|patch| {
        let mut map = JsonObject::new();
        for (key, value) in patch {
            map.insert(key, value.map_or(Value::Null, Value::String));
        }
        map
    });
    let (removed, added) = match tools {
        Some(changes) => (changes.removed, changes.added),
        None => (Vec::new(), Vec::new()),
    };
    let message = SystemMessage {
        content: UserContent::Text(String::new()),
        sections,
        tools_added: (!added.is_empty()).then_some(added),
        tools_removed: (!removed.is_empty()).then_some(removed),
        timestamp: timestamp as i64,
    };
    EntryDraft { model: Some(vec![Message::System(message)]), ..EntryDraft::new(SYSTEM_ENTRY.kind()) }
}
