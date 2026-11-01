//! SDK `SettingsManager` state and write queue (upstream `save`, `enqueueWrite` and
//! `persistScopedSettings`). A save records a snapshot; `sdk_settings.js` runs the queue that writes it.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Map, Value};

use super::js;
use super::manager::{SettingsError, State};
use super::schema::parse_settings;
use super::storage::{SettingsEdit, SettingsScope, SettingsStorage};

pub(super) const UNTRUSTED: &str = "Project is not trusted; refusing to write project settings";

/// Fields and nested keys changed since the last write of a scope, in insertion order.
#[derive(Clone, Default)]
pub(super) struct Modified {
    fields: Vec<String>,
    nested: Vec<(String, Vec<String>)>,
}

impl Modified {
    fn mark(&mut self, field: &str, nested: Option<&str>) {
        if !self.fields.iter().any(|known| known == field) {
            self.fields.push(field.to_string());
        }
        let Some(key) = nested.filter(|key| !key.is_empty()) else { return };
        let position = match self.nested.iter().position(|(known, _)| known == field) {
            Some(position) => position,
            None => {
                self.nested.push((field.to_string(), Vec::new()));
                self.nested.len() - 1
            }
        };
        let keys = &mut self.nested[position].1;
        if !keys.iter().any(|known| known == key) {
            keys.push(key.to_string());
        }
    }

    fn nested(&self, field: &str) -> Option<&[String]> {
        self.nested.iter().find(|(known, _)| known == field).map(|(_, keys)| keys.as_slice())
    }
}

/// One upstream `save`: the scope's layer and modified fields when it was called.
pub(super) struct Snapshot {
    pub scope: SettingsScope,
    layer: Map<String, Value>,
    modified: Modified,
}

/// The SDK part of the manager state: the raw merged settings with overrides (upstream `settings`)
/// and the queued snapshots. A `host` storage is read and written by JavaScript `withLock` calls.
pub(super) struct Deferred {
    pub host: bool,
    pub merged: Map<String, Value>,
    pub modified: [Modified; 2],
    pub pending: VecDeque<Snapshot>,
    pub active: Option<Snapshot>,
    pub unclaimed: usize,
    pub notify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The JavaScript error token of each entry of `State::errors`.
    pub tokens: Vec<Option<u64>>,
    /// `applyOverrides` objects merged since the last recompute, and the alias generations of
    /// `sdk_alias.rs`: per layer, per object path, and of the merged settings.
    pub overrides: Vec<Map<String, Value>>,
    pub epoch: [u64; 2],
    pub generations: HashMap<(usize, String), u64>,
    pub merged_epoch: u64,
}

impl Deferred {
    pub fn new(host: bool) -> Self {
        Self {
            host,
            merged: Map::new(),
            modified: Default::default(),
            pending: VecDeque::new(),
            active: None,
            unclaimed: 0,
            notify: None,
            tokens: Vec::new(),
            overrides: Vec::new(),
            epoch: [0; 2],
            generations: HashMap::new(),
            merged_epoch: 0,
        }
    }
}

/// A storage read: the text (`None` when there is none), or the error the read threw.
pub(crate) type Read = Result<Option<Value>, Thrown>;

/// An error as `drainErrors` reports it; `token` names the JavaScript error object it came from.
pub(crate) struct Thrown {
    pub message: String,
    pub token: Option<u64>,
}

impl From<String> for Thrown {
    fn from(message: String) -> Self {
        Self { message, token: None }
    }
}

/// Storage that `sdk_settings.js` drives; Rust never reads it.
pub(crate) struct HostStorage;

impl SettingsStorage for HostStorage {
    fn with_lock(&self, _scope: SettingsScope, _edit: SettingsEdit<'_>) -> Result<(), String> {
        Err("this SettingsManager storage is read and written by JavaScript".into())
    }
}

pub(super) fn slot(scope: SettingsScope) -> usize {
    match scope {
        SettingsScope::Global => 0,
        SettingsScope::Project => 1,
    }
}

pub(super) fn sdk(state: &mut State) -> &mut Deferred {
    state.sdk.as_mut().expect("an SDK settings manager")
}

pub(super) fn layer(state: &mut State, scope: SettingsScope) -> &mut Map<String, Value> {
    match scope {
        SettingsScope::Global => &mut state.global,
        SettingsScope::Project => &mut state.project,
    }
}

pub(super) fn set_failed(state: &mut State, scope: SettingsScope, failed: bool) {
    match scope {
        SettingsScope::Global => state.global_load_failed = failed,
        SettingsScope::Project => state.project_load_failed = failed,
    }
}

pub(super) fn record(state: &mut State, scope: SettingsScope, path: Option<PathBuf>, thrown: Thrown) {
    state.errors.push(SettingsError { scope, path, message: thrown.message });
    sdk(state).tokens.push(thrown.token);
}

/// Upstream `loadFromStorage` after the read: no text gives `{}`, other text is parsed and migrated.
pub(super) fn load(read: Read) -> Result<Map<String, Value>, Thrown> {
    match read? {
        None => Ok(Map::new()),
        Some(text) if !js::truthy(Some(&text)) => Ok(Map::new()),
        Some(Value::String(text)) => js::parse(&text).map_err(Thrown::from),
        Some(_) => Err(Thrown::from("settings text must be a string".to_string())),
    }
}

/// The text a snapshot writes over `current`: its modified fields, and for nested fields only
/// the modified keys, replace those of the current text.
pub(super) fn persist_text(current: Option<&str>, snapshot: &Snapshot) -> Result<String, String> {
    let file = match current.filter(|text| !text.is_empty()) {
        Some(text) => js::parse(text)?,
        None => Map::new(),
    };
    let mut merged = file.clone();
    merged.shift_remove(js::ARRAY);
    for field in &snapshot.modified.fields {
        let value = snapshot.layer.get(field);
        let entry = match snapshot.modified.nested(field) {
            Some(keys) if js::is_object_like(value) => {
                let mut nested = js::spread(file.get(field));
                for key in keys {
                    nested.insert(key.clone(), js::read(member(value, key)));
                }
                Value::Object(nested)
            }
            _ => js::read(value),
        };
        merged.insert(field.clone(), entry);
    }
    Ok(js::stringify(&merged))
}

fn member<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match value? {
        Value::Array(items) => key.parse::<usize>().ok().and_then(|index| items.get(index)),
        other => js::object(Some(other))?.get(key),
    }
}

/// The `current` text a host storage passed to the `withLock` callback.
pub(super) fn host_text(current: Option<Value>) -> Result<Option<String>, String> {
    match current {
        Some(Value::String(text)) => Ok(Some(text)),
        value if !js::truthy(value.as_ref()) => Ok(None),
        _ => Err("settings text must be a string".into()),
    }
}

/// `settings = deepMergeSettings(global, project)`; overrides are dropped, as upstream does.
pub(super) fn recompute(state: &mut State) {
    let merged = js::merge(&state.global, &state.project);
    let sdk = sdk(state);
    sdk.merged = merged;
    sdk.overrides.clear();
    sdk.merged_epoch += 1;
    retype(state);
}

/// The typed views native readers use: `undefined` keys are left out.
pub(super) fn retype(state: &mut State) {
    let merged = state.sdk.as_ref().map(|sdk| js::to_json_map(&sdk.merged)).unwrap_or_default();
    state.merged = parse_settings(&merged).0;
    state.global_typed = parse_settings(&js::to_json_map(&state.global)).0;
    state.project_typed = parse_settings(&js::to_json_map(&state.project)).0;
}

/// Upstream `applyOverrides`: merged over the current settings until the next recompute.
pub(super) fn apply_overrides(state: &mut State, overrides: Map<String, Value>) {
    let sdk = sdk(state);
    sdk.merged = js::merge(&sdk.merged, &overrides);
    sdk.overrides.push(overrides);
    sdk.merged_epoch += 1;
    retype(state);
}

pub(super) fn mark(state: &mut State, scope: SettingsScope, field: &str, nested: Option<&str>) {
    sdk(state).modified[slot(scope)].mark(field, nested);
}

/// Upstream `save` and `saveProjectSettings`: recompute, then queue a snapshot unless the scope
/// failed to load. `notify` wakes the JavaScript queue for writes made outside a JavaScript call.
pub(super) fn save(state: &mut State, scope: SettingsScope, notify: bool) {
    recompute(state);
    let failed = match scope {
        SettingsScope::Global => state.global_load_failed,
        SettingsScope::Project => state.project_load_failed,
    };
    if failed {
        return;
    }
    let layer = layer(state, scope).clone();
    let sdk = sdk(state);
    let modified = sdk.modified[slot(scope)].clone();
    sdk.pending.push_back(Snapshot { scope, layer, modified });
    sdk.unclaimed += 1;
    if let Some(wake) = sdk.notify.as_ref().filter(|_| notify) {
        wake();
    }
}
