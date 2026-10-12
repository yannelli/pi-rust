//! Rebuild model context from the entry tree: follow the active branch, apply the latest
//! compaction, then apply `context_edit` entries. Port of upstream `buildSessionProjection`.

use std::collections::HashMap;

use pi_ai::{ContentBlock, Message, ThinkingLevel, UserContent};
use serde_json::Value;

use super::entry::{ContextReplacement, Entry, EntryData};
use crate::messages::{create_branch_summary_message, create_compaction_summary_message, create_custom_message};

/// Which entry a path ends at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leaf<'a> {
    /// The last entry in file order.
    Last,
    /// Before the first entry: an empty path.
    Root,
    /// This entry, or the last entry when the id is unknown.
    Entry(&'a str),
}

impl<'a> From<Option<&'a str>> for Leaf<'a> {
    fn from(leaf: Option<&'a str>) -> Self {
        leaf.map_or(Leaf::Root, Leaf::Entry)
    }
}

/// Messages, model, and thinking level rebuilt from the current branch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionContext {
    pub messages: Vec<Message>,
    /// `(provider, model id)` from the latest model change or assistant message.
    pub model: Option<(String, String)>,
    /// The latest thinking level change, if any (upstream reports `off` when absent).
    pub thinking_level: Option<ThinkingLevel>,
}

/// One selected entry and the messages it contributes after context edits.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedEntry<'a> {
    pub source: &'a Entry,
    /// Empty for state-only entries and omitted targets.
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionProjection<'a> {
    pub entries: Vec<ProjectedEntry<'a>>,
    pub messages: Vec<Message>,
    pub model: Option<(String, String)>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl SessionProjection<'_> {
    pub fn into_context(self) -> SessionContext {
        SessionContext { messages: self.messages, model: self.model, thinking_level: self.thinking_level }
    }
}

/// Entries from the root to `leaf`, following `parentId` links.
pub fn build_session_path<'a>(entries: &'a [Entry], leaf: Leaf<'_>) -> Vec<&'a Entry> {
    let index: HashMap<&str, &Entry> = entries.iter().map(|entry| (entry.id(), entry)).collect();
    let start = match leaf {
        Leaf::Root => return Vec::new(),
        Leaf::Last => entries.last(),
        Leaf::Entry(id) => index.get(id).copied().or(entries.last()),
    };
    let mut path = Vec::new();
    let mut current = start;
    while let Some(entry) = current {
        path.push(entry);
        if path.len() > entries.len() {
            break;
        }
        current = entry.parent_id().and_then(|parent| index.get(parent).copied());
    }
    path.reverse();
    path
}

/// The path from the root to the last of `entries`, following `parentId` within them.
pub fn path_to_last<'a>(entries: &[&'a Entry]) -> Vec<&'a Entry> {
    let index: HashMap<&str, &Entry> = entries.iter().map(|entry| (entry.id(), *entry)).collect();
    let mut path = Vec::new();
    let mut current = entries.last().copied();
    while let Some(entry) = current {
        path.push(entry);
        if path.len() > entries.len() {
            break;
        }
        current = entry.parent_id().and_then(|parent| index.get(parent).copied());
    }
    path.reverse();
    path
}

/// The latest compaction entry in `entries`.
pub fn latest_compaction<'a>(entries: &[&'a Entry]) -> Option<&'a Entry> {
    entries.iter().rev().find(|entry| entry.compaction().is_some()).copied()
}

/// The active entries: the latest compaction first, then its kept non-system entries, then later entries.
pub fn build_context_entries<'a>(entries: &'a [Entry], leaf: Leaf<'_>) -> Vec<&'a Entry> {
    context_entries_of_path(build_session_path(entries, leaf))
}

pub(crate) fn context_entries_of_path(path: Vec<&Entry>) -> Vec<&Entry> {
    let Some(compaction_index) = path.iter().rposition(|entry| entry.compaction().is_some()) else { return path };
    let compaction = path[compaction_index];
    let first_kept = compaction.compaction().map(|data| data.first_kept_entry_id.as_str()).unwrap_or_default();
    let mut selected = vec![compaction];
    let mut found = false;
    for entry in &path[..compaction_index] {
        found |= entry.id() == first_kept;
        if found && !entry.is_message_role("system") {
            selected.push(entry);
        }
    }
    selected.extend(&path[compaction_index + 1..]);
    selected
}

/// Messages one entry contributes to context, before context edits.
pub fn session_entry_to_context_messages(entry: &Entry) -> Vec<Message> {
    match entry.data() {
        EntryData::Message(message) => vec![message.clone()],
        EntryData::CustomMessage(custom) => vec![create_custom_message(
            &custom.custom_type,
            custom.content.clone(),
            custom.display,
            custom.details.clone(),
            entry.timestamp(),
        )],
        EntryData::BranchSummary(summary) if !summary.summary.is_empty() => {
            vec![create_branch_summary_message(&summary.summary, &summary.from_id, entry.timestamp())]
        }
        EntryData::Compaction(compaction) => {
            let summary =
                create_compaction_summary_message(&compaction.summary, compaction.tokens_before, entry.timestamp());
            compaction.system_message.clone().map(Message::System).into_iter().chain([summary]).collect()
        }
        _ => Vec::new(),
    }
}

fn block_content(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::String(text) => vec![ContentBlock::text(text.clone())],
        other => serde_json::from_value(other.clone()).unwrap_or_default(),
    }
}

fn replace_content(message: Message, replacement: &ContextReplacement) -> Message {
    match message {
        Message::User(mut user) => {
            user.content =
                serde_json::from_value(replacement.content.clone()).unwrap_or(UserContent::Blocks(Vec::new()));
            Message::User(user)
        }
        Message::Assistant(mut assistant) => {
            assistant.content = block_content(&replacement.content);
            Message::Assistant(assistant)
        }
        Message::ToolResult(mut result) => {
            result.content = block_content(&replacement.content);
            Message::ToolResult(result)
        }
        Message::Custom(mut custom) if custom.role == "custom" => {
            custom.fields.insert("content".into(), replacement.content.clone());
            Message::Custom(custom)
        }
        other => other,
    }
}

fn project_entry(entry: &Entry, edit: Option<&Entry>) -> Vec<Message> {
    let messages = session_entry_to_context_messages(entry);
    let Some(EntryData::ContextEdit { replacement, .. }) = edit.map(Entry::data) else { return messages };
    let Some(replacement) = replacement else { return Vec::new() };
    messages.into_iter().map(|message| replace_content(message, replacement)).collect()
}

/// Model and thinking level from the whole path, as upstream `getSessionContextSettings`.
pub fn context_settings(path: &[&Entry]) -> (Option<(String, String)>, Option<ThinkingLevel>) {
    let (mut model, mut thinking_level) = (None, None);
    for entry in path {
        match entry.data() {
            EntryData::ThinkingLevelChange { thinking_level: level } => {
                thinking_level = level.parse::<ThinkingLevel>().ok().or(thinking_level);
            }
            EntryData::ModelChange { provider, model_id } => model = Some((provider.clone(), model_id.clone())),
            EntryData::Message(Message::Assistant(assistant)) => {
                model = Some((assistant.provider.clone(), assistant.model.clone()));
            }
            _ => {}
        }
    }
    (model, thinking_level)
}

/// Provenance-preserving, compaction-aware model context for `leaf`.
pub fn build_session_projection<'a>(entries: &'a [Entry], leaf: Leaf<'_>) -> SessionProjection<'a> {
    project_path(build_session_path(entries, leaf))
}

pub(crate) fn project_path(path: Vec<&Entry>) -> SessionProjection<'_> {
    let (model, thinking_level) = context_settings(&path);
    let selected = context_entries_of_path(path);
    let mut edits: HashMap<&str, &Entry> = HashMap::new();
    for entry in &selected {
        if let EntryData::ContextEdit { target_id, .. } = entry.data() {
            edits.insert(target_id.as_str(), entry);
        }
    }
    let entries: Vec<ProjectedEntry> = selected
        .iter()
        .enumerate()
        .map(|(index, entry)| ProjectedEntry {
            source: entry,
            // Only the newest compaction, projected first, contributes a checkpoint and summary.
            messages: if entry.compaction().is_some() && index > 0 {
                Vec::new()
            } else {
                project_entry(entry, edits.get(entry.id()).copied())
            },
        })
        .collect();
    let messages = entries.iter().flat_map(|entry| entry.messages.iter().cloned()).collect();
    SessionProjection { entries, messages, model, thinking_level }
}

/// The finalized model context for `leaf`.
pub fn build_session_context(entries: &[Entry], leaf: Leaf<'_>) -> SessionContext {
    build_session_projection(entries, leaf).into_context()
}
