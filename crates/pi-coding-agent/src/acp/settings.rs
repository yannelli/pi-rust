//! The pi settings ACP mode reads before a session exists (upstream `pi-settings.ts`): the
//! global and project `settings.json` merged, with project values winning.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

fn read_object(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(object) => Some(object),
            _ => None,
        })
        .unwrap_or_default()
}

fn deep_merge(mut base: Map<String, Value>, overlay: Map<String, Value>) -> Map<String, Value> {
    for (key, value) in overlay {
        match (base.remove(&key), value) {
            (Some(Value::Object(left)), Value::Object(right)) => {
                base.insert(key, Value::Object(deep_merge(left, right)));
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
    base
}

/// `<agent_dir>/settings.json` merged with `<cwd>/.pi/settings.json`.
pub fn merged_settings(cwd: &Path, agent_dir: &Path) -> Map<String, Value> {
    let global = read_object(&agent_dir.join("settings.json"));
    let project = read_object(&cwd.join(".pi").join("settings.json"));
    deep_merge(global, project)
}

/// `enableSkillCommands` (or the older `skills.enableSkillCommands`); on by default.
pub fn enable_skill_commands(cwd: &Path, agent_dir: &Path) -> bool {
    let merged = merged_settings(cwd, agent_dir);
    if let Some(flag) = merged.get("enableSkillCommands").and_then(Value::as_bool) {
        return flag;
    }
    merged.get("skills").and_then(|skills| skills.get("enableSkillCommands")).and_then(Value::as_bool).unwrap_or(true)
}

/// `quietStartup: true` (or the older `quietStart`) hides the startup info block.
pub fn quiet_startup(cwd: &Path, agent_dir: &Path) -> bool {
    let merged = merged_settings(cwd, agent_dir);
    merged.get("quietStartup").and_then(Value::as_bool).or_else(|| merged.get("quietStart").and_then(Value::as_bool))
        == Some(true)
}

/// The `sessionDir` of the global settings, relative paths resolved against `agent_dir`.
pub fn settings_session_dir(agent_dir: &Path) -> Option<PathBuf> {
    let settings = read_object(&agent_dir.join("settings.json"));
    let dir = settings.get("sessionDir").and_then(Value::as_str).map(str::trim).filter(|dir| !dir.is_empty())?;
    let dir = crate::config::expand_tilde(dir);
    Some(if dir.is_absolute() { dir } else { agent_dir.join(dir) })
}

/// Where pi keeps sessions: the override, the `sessionDir` setting, or `<agent_dir>/sessions`.
pub fn sessions_root(agent_dir: &Path, dir_override: Option<&Path>) -> PathBuf {
    dir_override
        .map(Path::to_path_buf)
        .or_else(|| settings_session_dir(agent_dir))
        .unwrap_or_else(|| agent_dir.join("sessions"))
}
