//! Deferred context (pi-rust extension): skills and extension context entries, such as agent
//! configs, that stay out of the system prompt until the model finds them with `tool_search`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pi_codemode::ContextSearchEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Skill;

/// The kind of skills in `contextExposure` and in `tool_search` results.
pub const SKILL_KIND: &str = "skill";

/// How a skill or context entry reaches the model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextExposure {
    /// Listed in the system prompt.
    #[default]
    Direct,
    /// Left out of the system prompt, counted in a hint, and found with `tool_search`.
    Deferred,
}

impl ContextExposure {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "direct" => Some(Self::Direct),
            "deferred" => Some(Self::Deferred),
            _ => None,
        }
    }
}

/// An item of model-facing context that an extension contributes with `resources_discover`
/// (`contextEntries`), such as an agent config of the subagents extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextEntry {
    /// Lowercase kind, such as `agent`. The `contextExposure` setting is keyed by it.
    pub kind: String,
    pub name: String,
    pub description: String,
    /// How the model loads or uses the entry, for example which tool to call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// A file the model reads to load the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Overrides the `contextExposure` setting of the kind for this entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ContextExposure>,
    /// The extension that contributed the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_path: Option<String>,
}

impl ContextEntry {
    /// What `tool_search` returns for the entry: its usage, then the file to read.
    pub fn search_entry(&self) -> ContextSearchEntry {
        let read = self.path.as_ref().map(|path| format!("Read {} to load it.", path.display()));
        let usage = [self.usage.clone(), read].into_iter().flatten().collect::<Vec<_>>().join(" ");
        ContextSearchEntry {
            kind: self.kind.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            usage: (!usage.is_empty()).then_some(usage),
        }
    }
}

fn is_valid_kind(kind: &str) -> bool {
    let mut chars = kind.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

/// Entries of the `contextEntries` lists `resources_discover` returned. Entries without a valid
/// kind, a name, or a description are dropped; the first entry of a kind and name wins.
pub fn context_entries_from_json(found: &Value) -> Vec<ContextEntry> {
    let mut entries: Vec<ContextEntry> = Vec::new();
    for item in found.as_array().into_iter().flatten() {
        let Ok(mut entry) = serde_json::from_value::<ContextEntry>(item.clone()) else { continue };
        entry.name = entry.name.trim().to_string();
        entry.description = entry.description.trim().to_string();
        if !is_valid_kind(&entry.kind) || entry.name.is_empty() || entry.description.is_empty() {
            continue;
        }
        if !entries.iter().any(|known| known.kind == entry.kind && known.name == entry.name) {
            entries.push(entry);
        }
    }
    entries
}

/// The exposure of an item: its own, else the setting of its kind, else `direct`.
pub fn resolve_exposure(
    item: Option<ContextExposure>,
    kind: &str,
    settings: &BTreeMap<String, ContextExposure>,
) -> ContextExposure {
    item.or_else(|| settings.get(kind).copied()).unwrap_or_default()
}

pub fn skill_exposure(skill: &Skill, settings: &BTreeMap<String, ContextExposure>) -> ContextExposure {
    resolve_exposure(skill.exposure, SKILL_KIND, settings)
}

pub fn entry_exposure(entry: &ContextEntry, settings: &BTreeMap<String, ContextExposure>) -> ContextExposure {
    resolve_exposure(entry.exposure, &entry.kind, settings)
}

/// What `tool_search` returns for a skill: the file to read.
pub fn skill_search_entry(skill: &Skill) -> ContextSearchEntry {
    let usage = format!(
        "Read {} to load its instructions. Resolve relative paths in it against {}.",
        skill.file_path.display(),
        skill.base_dir.display()
    );
    ContextSearchEntry {
        kind: SKILL_KIND.into(),
        name: skill.name.clone(),
        description: skill.description.clone(),
        usage: Some(usage),
    }
}

/// The deferred skills and entries `tool_search` can find. Skills the model may not invoke are left out.
pub fn deferred_search_entries(
    skills: &[Skill],
    entries: &[ContextEntry],
    settings: &BTreeMap<String, ContextExposure>,
) -> Vec<ContextSearchEntry> {
    let skills = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation && skill_exposure(skill, settings) == ContextExposure::Deferred)
        .map(skill_search_entry);
    let entries = entries
        .iter()
        .filter(|entry| entry_exposure(entry, settings) == ContextExposure::Deferred)
        .map(ContextEntry::search_entry);
    skills.chain(entries).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_valid_entries_once() {
        let found = json!([
            { "kind": "agent", "name": " reviewer ", "description": "Reviews diffs.", "usage": "Call subagent.", "extensionPath": "/ext" },
            { "kind": "agent", "name": "reviewer", "description": "Second copy." },
            { "kind": "Agent", "name": "bad-kind", "description": "x" },
            { "kind": "agent", "name": "no-description", "description": " " },
            { "kind": "agent", "name": "deferred", "description": "x", "exposure": "deferred", "path": "/a.md" },
            { "kind": "agent", "name": "bad-exposure", "description": "x", "exposure": "hidden" },
        ]);
        let entries = context_entries_from_json(&found);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["reviewer", "deferred"]);
        assert_eq!(entries[0].description, "Reviews diffs.");
        assert_eq!(entries[0].extension_path.as_deref(), Some("/ext"));
        assert_eq!(entries[1].exposure, Some(ContextExposure::Deferred));
        assert_eq!(entries[1].search_entry().usage.as_deref(), Some("Read /a.md to load it."));
        assert_eq!(entries[0].search_entry().usage.as_deref(), Some("Call subagent."));
    }

    #[test]
    fn item_exposure_wins_over_the_kind_setting() {
        let settings = BTreeMap::from([("agent".to_string(), ContextExposure::Deferred)]);
        assert_eq!(resolve_exposure(None, "agent", &settings), ContextExposure::Deferred);
        assert_eq!(resolve_exposure(Some(ContextExposure::Direct), "agent", &settings), ContextExposure::Direct);
        assert_eq!(resolve_exposure(None, "skill", &settings), ContextExposure::Direct);
        assert_eq!(resolve_exposure(None, "skill", &BTreeMap::new()), ContextExposure::Direct);
    }
}
