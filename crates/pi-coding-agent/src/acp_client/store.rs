//! Read and write the `acpAgents` settings key in the global and project settings files.
//! Writes edit the raw array, so entries that do not parse and unknown fields stay as they are.

use serde_json::Value;

use super::entry::{AcpAgentEntry, AcpScope};
use crate::settings::SettingsManager;

pub const SETTINGS_KEY: &str = "acpAgents";

fn raw(settings: &SettingsManager, scope: AcpScope) -> Vec<Value> {
    let layer = match scope {
        AcpScope::Global => settings.global_settings(),
        AcpScope::Project => settings.project_settings(),
    };
    match layer.extra.get(SETTINGS_KEY) {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// The entries of one scope that parse.
pub fn scope_entries(settings: &SettingsManager, scope: AcpScope) -> Vec<AcpAgentEntry> {
    raw(settings, scope).into_iter().filter_map(|item| serde_json::from_value(item).ok()).collect()
}

/// Global and project entries. A project entry replaces the global entry with the same ID.
pub fn list(settings: &SettingsManager) -> Vec<(AcpAgentEntry, AcpScope)> {
    let mut entries: Vec<(AcpAgentEntry, AcpScope)> =
        scope_entries(settings, AcpScope::Global).into_iter().map(|entry| (entry, AcpScope::Global)).collect();
    for entry in scope_entries(settings, AcpScope::Project) {
        match entries.iter_mut().find(|(existing, _)| existing.id == entry.id) {
            Some(slot) => *slot = (entry, AcpScope::Project),
            None => entries.push((entry, AcpScope::Project)),
        }
    }
    entries
}

/// Every entry of both scopes, with whether a project entry replaces it.
pub fn all(settings: &SettingsManager) -> Vec<(AcpAgentEntry, AcpScope, bool)> {
    let project = scope_entries(settings, AcpScope::Project);
    let global = scope_entries(settings, AcpScope::Global).into_iter().map(|entry| {
        let shadowed = project.iter().any(|other| other.id == entry.id);
        (entry, AcpScope::Global, shadowed)
    });
    let global: Vec<_> = global.collect();
    global.into_iter().chain(project.into_iter().map(|entry| (entry, AcpScope::Project, false))).collect()
}

/// The entry `id` of `scope`.
pub fn get(settings: &SettingsManager, id: &str, scope: AcpScope) -> Option<AcpAgentEntry> {
    scope_entries(settings, scope).into_iter().find(|entry| entry.id == id)
}

/// Moves the entry `id` from `from` to the other scope, which must not have one.
pub fn move_entry(settings: &SettingsManager, id: &str, from: AcpScope) -> Result<AcpScope, String> {
    let to = match from {
        AcpScope::Global => AcpScope::Project,
        AcpScope::Project => AcpScope::Global,
    };
    if get(settings, id, to).is_some() {
        return Err(format!("The {} settings already have an ACP agent \"{id}\"", to.label()));
    }
    let Some(item) = raw(settings, from).into_iter().find(|item| has_id(item, id)) else {
        return Err(format!("ACP agent \"{id}\" is not in the {} settings", from.label()));
    };
    let mut target = raw(settings, to);
    target.push(item);
    write(settings, to, target)?;
    remove(settings, id, from)?;
    Ok(to)
}

fn has_id(item: &Value, id: &str) -> bool {
    item.get("id").and_then(Value::as_str) == Some(id)
}

fn write(settings: &SettingsManager, scope: AcpScope, items: Vec<Value>) -> Result<(), String> {
    let value = (!items.is_empty()).then_some(Value::Array(items));
    match scope {
        AcpScope::Global => {
            settings.set_global_value(SETTINGS_KEY, value);
            Ok(())
        }
        AcpScope::Project => settings.set_project_value(SETTINGS_KEY, value),
    }
}

/// Add `entry`, or replace the entry with its ID.
pub fn upsert(settings: &SettingsManager, entry: &AcpAgentEntry, scope: AcpScope) -> Result<(), String> {
    let value = serde_json::to_value(entry).map_err(|error| error.to_string())?;
    let mut items = raw(settings, scope);
    match items.iter_mut().find(|item| has_id(item, &entry.id)) {
        Some(slot) => *slot = value,
        None => items.push(value),
    }
    write(settings, scope, items)
}

/// Remove the entry with `id` and return it when it parsed.
pub fn remove(settings: &SettingsManager, id: &str, scope: AcpScope) -> Result<Option<AcpAgentEntry>, String> {
    let (removed, kept): (Vec<Value>, Vec<Value>) = raw(settings, scope).into_iter().partition(|item| has_id(item, id));
    if removed.is_empty() {
        return Ok(None);
    }
    write(settings, scope, kept)?;
    Ok(removed.into_iter().find_map(|item| serde_json::from_value(item).ok()))
}

pub fn set_enabled(settings: &SettingsManager, id: &str, scope: AcpScope, enabled: bool) -> Result<(), String> {
    let mut items = raw(settings, scope);
    let Some(Value::Object(object)) = items.iter_mut().find(|item| has_id(item, id)) else {
        return Err(format!("ACP agent \"{id}\" is not in the {} settings", scope.label()));
    };
    object.insert("enabled".into(), Value::Bool(enabled));
    write(settings, scope, items)
}
