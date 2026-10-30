//! Loading, merging, and saving settings (upstream `core/settings-manager.ts`). Project
//! settings override global ones, nested objects merge, arrays replace. Writes change only
//! the touched keys of the locked file, so unknown keys and other processes' edits survive.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard};

use serde_json::{Map, Value};

use super::schema::{Settings, parse_settings};
use super::storage::{FileSettingsStorage, InMemorySettingsStorage, SettingsScope, SettingsStorage};
use super::tools::merge_default_tools;

/// Options for [`SettingsManager::create`].
#[derive(Clone, Copy, Debug)]
pub struct SettingsOptions {
    /// Untrusted projects contribute no project settings and cannot be written.
    pub project_trusted: bool,
}

impl Default for SettingsOptions {
    fn default() -> Self {
        Self { project_trusted: true }
    }
}

/// A settings file that could not be read or written, or a key that does not match the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsError {
    pub scope: SettingsScope,
    pub path: Option<PathBuf>,
    pub message: String,
}

impl SettingsError {
    /// Warning text in upstream form (`collectSettingsDiagnostics`).
    pub fn diagnostic(&self) -> String {
        match &self.path {
            Some(path) => format!("Invalid settings file {}: {}", path.display(), self.message),
            None => format!("Invalid {} settings: {}", self.scope.as_str(), self.message),
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub global: Map<String, Value>,
    pub project: Map<String, Value>,
    pub overrides: Map<String, Value>,
    pub merged: Settings,
    pub global_typed: Settings,
    pub project_typed: Settings,
    pub project_trusted: bool,
    pub global_load_failed: bool,
    pub project_load_failed: bool,
    pub errors: Vec<SettingsError>,
}

/// Global and project settings with typed accessors. Shared by reference; methods take `&self`.
pub struct SettingsManager {
    storage: Arc<dyn SettingsStorage>,
    pub(super) state: RwLock<State>,
}

fn deep_merge(base: &mut Map<String, Value>, overrides: &Map<String, Value>) {
    for (key, value) in overrides {
        match (base.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(update)) => deep_merge(existing, update),
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_array().map(|items| items.iter().filter_map(|item| item.as_str().map(String::from)).collect())
}

/// Merge two layers as upstream `deepMergeSettings` does, including `defaultTools` deltas.
pub(super) fn merge_layers(base: &Map<String, Value>, overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = base.clone();
    deep_merge(&mut merged, overrides);
    let tools = merge_default_tools(
        string_list(base.get("defaultTools")).as_deref(),
        string_list(overrides.get("defaultTools")).as_deref(),
    );
    if let Some(tools) = tools.filter(|_| overrides.get("defaultTools").is_none_or(Value::is_array)) {
        merged.insert("defaultTools".into(), Value::from(tools));
    }
    merged
}

/// Upstream `migrateSettings`: legacy keys move to their current names.
pub(super) fn migrate(settings: &mut Map<String, Value>) {
    if !settings.contains_key("steeringMode")
        && let Some(mode) = settings.remove("queueMode")
    {
        settings.insert("steeringMode".into(), mode);
    }
    if !settings.contains_key("transport")
        && let Some(Value::Bool(websockets)) = settings.get("websockets").cloned()
    {
        settings.insert("transport".into(), Value::from(if websockets { "websocket" } else { "sse" }));
        settings.remove("websockets");
    }
    if let Some(Value::Object(skills)) = settings.get("skills").cloned() {
        if let Some(enable) = skills.get("enableSkillCommands").filter(|value| !value.is_null())
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".into(), enable.clone());
        }
        match skills.get("customDirectories") {
            Some(Value::Array(directories)) if !directories.is_empty() => {
                settings.insert("skills".into(), Value::Array(directories.clone()));
            }
            _ => {
                settings.remove("skills");
            }
        }
    }
    if let Some(Value::Object(retry)) = settings.get_mut("retry") {
        let legacy = retry.remove("maxDelayMs");
        if let Some(delay @ Value::Number(_)) = legacy {
            let provider = retry.entry("provider").or_insert_with(|| Value::Object(Map::new()));
            if let Value::Object(provider) = provider
                && provider.get("maxRetryDelayMs").is_none_or(Value::is_null)
            {
                provider.insert("maxRetryDelayMs".into(), delay);
            }
        }
    }
}

/// Parse settings text: a byte order mark is ignored and legacy keys migrate.
pub(super) fn parse_text(text: &str) -> Result<Map<String, Value>, String> {
    let text = text.trim_start_matches('\u{feff}');
    match serde_json::from_str::<Value>(text).map_err(|error| error.to_string())? {
        Value::Object(mut map) => {
            migrate(&mut map);
            Ok(map)
        }
        _ => Err("expected a JSON object".into()),
    }
}

fn set_key(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    match value {
        Some(value) => {
            map.insert(key.to_string(), value);
        }
        None => {
            map.remove(key);
        }
    }
}

impl SettingsManager {
    /// Settings of `<agent dir>/settings.json` and `<cwd>/.pi/settings.json`.
    pub fn create(cwd: &Path, agent_dir: &Path, options: SettingsOptions) -> Self {
        Self::from_storage(Arc::new(FileSettingsStorage::new(cwd, agent_dir)), options)
    }

    pub fn from_storage(storage: Arc<dyn SettingsStorage>, options: SettingsOptions) -> Self {
        let state = State { project_trusted: options.project_trusted, ..State::default() };
        let manager = Self { storage, state: RwLock::new(state) };
        manager.load(SettingsScope::Global);
        manager.load(SettingsScope::Project);
        manager.recompute();
        manager
    }

    /// In-memory settings with `settings` as the global layer.
    pub fn in_memory(settings: Value) -> Self {
        let storage = InMemorySettingsStorage::default();
        let text = serde_json::to_string_pretty(&settings).unwrap_or_else(|_| "{}".into());
        let _ = storage.with_lock(SettingsScope::Global, &mut |_| Ok(Some(text.clone())));
        Self::from_storage(Arc::new(storage), SettingsOptions::default())
    }

    fn read_scope(&self, scope: SettingsScope) -> Result<Map<String, Value>, String> {
        let mut content = None;
        self.storage.with_lock(scope, &mut |current| {
            content = current.map(String::from);
            Ok(None)
        })?;
        match content.filter(|text| !text.trim().is_empty()) {
            Some(text) => parse_text(&text),
            None => Ok(Map::new()),
        }
    }

    fn record(state: &mut State, scope: SettingsScope, path: Option<PathBuf>, message: String) {
        state.errors.push(SettingsError { scope, path, message });
    }

    /// Load one scope. A failed load keeps the earlier contents and blocks writes to it.
    fn load(&self, scope: SettingsScope) {
        let trusted = self.state().project_trusted;
        let result = if scope == SettingsScope::Project && !trusted { Ok(Map::new()) } else { self.read_scope(scope) };
        let path = self.storage.path(scope);
        let mut state = self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let failed = match result {
            Ok(map) => {
                for (key, message) in parse_settings(&map).1 {
                    Self::record(&mut state, scope, path.clone(), format!("\"{key}\": {message}"));
                }
                match scope {
                    SettingsScope::Global => state.global = map,
                    SettingsScope::Project => state.project = map,
                }
                false
            }
            Err(message) => {
                Self::record(&mut state, scope, path, message);
                true
            }
        };
        match scope {
            SettingsScope::Global => state.global_load_failed = failed,
            SettingsScope::Project => state.project_load_failed = failed,
        }
    }

    pub(super) fn recompute(&self) {
        let mut state = self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        let merged = merge_layers(&merge_layers(&state.global, &state.project), &state.overrides);
        state.merged = parse_settings(&merged).0;
        state.global_typed = parse_settings(&state.global).0;
        state.project_typed = parse_settings(&state.project).0;
    }

    pub(super) fn state(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Re-read both files.
    pub fn reload(&self) {
        self.load(SettingsScope::Global);
        self.load(SettingsScope::Project);
        self.recompute();
    }

    /// Effective settings: global, then project, then runtime overrides.
    pub fn settings(&self) -> Settings {
        self.state().merged.clone()
    }

    pub fn global_settings(&self) -> Settings {
        self.state().global_typed.clone()
    }

    pub fn project_settings(&self) -> Settings {
        self.state().project_typed.clone()
    }

    pub fn is_project_trusted(&self) -> bool {
        self.state().project_trusted
    }

    /// Changing trust reloads the project layer (or clears it).
    pub fn set_project_trusted(&self, trusted: bool) {
        if self.is_project_trusted() == trusted {
            return;
        }
        self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner()).project_trusted = trusted;
        self.load(SettingsScope::Project);
        self.recompute();
    }

    /// Overlay settings for this process only, for example `--use-theme`.
    pub fn apply_overrides(&self, overrides: Value) {
        if let Value::Object(overrides) = overrides {
            let mut state = self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            let combined = merge_layers(&state.overrides, &overrides);
            state.overrides = combined;
        }
        self.recompute();
    }

    /// Load and write problems since the last call.
    pub fn drain_errors(&self) -> Vec<SettingsError> {
        std::mem::take(&mut self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner()).errors)
    }

    /// Writes are synchronous; kept for parity with upstream callers.
    pub fn flush(&self) {}

    /// Change keys of one scope in memory and in its file. Skipped for a scope whose file
    /// failed to load, so an invalid file is never overwritten.
    pub(super) fn write(&self, scope: SettingsScope, edit: impl Fn(&mut Map<String, Value>)) -> Result<(), String> {
        {
            let mut state = self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            if scope == SettingsScope::Project && !state.project_trusted {
                return Err("Project is not trusted; refusing to write project settings".into());
            }
            match scope {
                SettingsScope::Global => edit(&mut state.global),
                SettingsScope::Project => edit(&mut state.project),
            }
        }
        self.recompute();
        let failed = {
            let state = self.state();
            if scope == SettingsScope::Global { state.global_load_failed } else { state.project_load_failed }
        };
        if failed {
            return Ok(());
        }
        let result = self.storage.with_lock(scope, &mut |current| {
            let mut file = match current.filter(|text| !text.trim().is_empty()) {
                Some(text) => parse_text(text)?,
                None => Map::new(),
            };
            edit(&mut file);
            serde_json::to_string_pretty(&file).map(Some).map_err(|error| error.to_string())
        });
        if let Err(message) = &result {
            let path = self.storage.path(scope);
            let mut state = self.state.write().unwrap_or_else(|poisoned| poisoned.into_inner());
            Self::record(&mut state, scope, path, message.clone());
        }
        result
    }

    /// Set (or with `None`, remove) a top-level global key.
    pub fn set_global_value(&self, key: &str, value: Option<Value>) {
        let _ = self.write(SettingsScope::Global, |map| set_key(map, key, value.clone()));
    }

    /// Set (or remove) `key.nested` in global settings, keeping the other nested keys.
    pub fn set_global_nested(&self, key: &str, nested: &str, value: Option<Value>) {
        let _ = self.write(SettingsScope::Global, |map| {
            let entry = map.entry(key.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !entry.is_object() {
                *entry = Value::Object(Map::new());
            }
            if let Value::Object(object) = entry {
                set_key(object, nested, value.clone());
            }
        });
    }

    /// Set (or remove) `key.nested.field` in global settings, keeping sibling keys. Objects
    /// that a removal leaves empty are removed too.
    pub fn set_global_nested2(&self, key: &str, nested: &str, field: &str, value: Option<Value>) {
        let _ = self.write(SettingsScope::Global, |map| {
            let removing = value.is_none();
            let outer = map.entry(key.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !outer.is_object() {
                *outer = Value::Object(Map::new());
            }
            let Value::Object(outer) = outer else { return };
            let inner = outer.entry(nested.to_string()).or_insert_with(|| Value::Object(Map::new()));
            if !inner.is_object() {
                *inner = Value::Object(Map::new());
            }
            let inner_empty = match inner {
                Value::Object(inner) => {
                    set_key(inner, field, value.clone());
                    inner.is_empty()
                }
                _ => false,
            };
            if removing && inner_empty {
                outer.remove(nested);
            }
            if removing && outer.is_empty() {
                map.remove(key);
            }
        });
    }

    /// Set (or remove) a top-level project key. Fails for untrusted projects.
    pub fn set_project_value(&self, key: &str, value: Option<Value>) -> Result<(), String> {
        self.write(SettingsScope::Project, |map| set_key(map, key, value.clone()))
    }
}
