//! Named keybinding actions (`tui.editor.cursorUp`, ...), their default keys, user overrides,
//! and conflict reporting. Applications extend [`tui_keybindings`] with their own ids.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use crate::keys::matches_key;

mod config;
mod definitions;

pub use config::{KeybindingValue, KeybindingsConfig};
pub use definitions::{KeybindingDefinition, KeybindingDefinitions, ids, tui_keybindings};

/// One key that the user assigned to more than one keybinding id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeybindingConflict {
    /// The key id claimed more than once.
    pub key: String,
    /// The keybinding ids that claim it, in config order.
    pub keybindings: Vec<String>,
}

/// Removes duplicate keys, keeping the first occurrence.
fn normalize_keys(keys: &[String]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        if !result.contains(key) {
            result.push(key.clone());
        }
    }
    result
}

/// Resolves keybinding ids to keys from definitions and user overrides.
#[derive(Debug, Clone)]
pub struct KeybindingsManager {
    definitions: KeybindingDefinitions,
    user_bindings: KeybindingsConfig,
    keys_by_id: HashMap<String, Vec<String>>,
    conflicts: Vec<KeybindingConflict>,
}

impl Default for KeybindingsManager {
    fn default() -> Self {
        Self::new(tui_keybindings(), KeybindingsConfig::default())
    }
}

impl KeybindingsManager {
    /// Creates a manager. A user value replaces the default keys for its id; an empty list
    /// disables the id. User values for unknown ids are ignored.
    pub fn new(definitions: KeybindingDefinitions, user_bindings: KeybindingsConfig) -> Self {
        let mut manager = Self { definitions, user_bindings, keys_by_id: HashMap::new(), conflicts: Vec::new() };
        manager.rebuild();
        manager
    }

    fn rebuild(&mut self) {
        self.keys_by_id.clear();
        self.conflicts.clear();

        let mut user_claims: Vec<(String, Vec<String>)> = Vec::new();
        for (id, value) in self.user_bindings.iter() {
            if !self.definitions.contains(id) {
                continue;
            }
            for key in normalize_keys(value.keys()) {
                match user_claims.iter_mut().find(|(claimed, _)| *claimed == key) {
                    Some((_, claimants)) if claimants.iter().any(|existing| existing == id) => {}
                    Some((_, claimants)) => claimants.push(id.to_string()),
                    None => user_claims.push((key, vec![id.to_string()])),
                }
            }
        }
        self.conflicts = user_claims
            .into_iter()
            .filter(|(_, claimants)| claimants.len() > 1)
            .map(|(key, keybindings)| KeybindingConflict { key, keybindings })
            .collect();

        for (id, definition) in self.definitions.iter() {
            let keys = match self.user_bindings.get(id) {
                Some(value) => normalize_keys(value.keys()),
                None => normalize_keys(&definition.default_keys),
            };
            self.keys_by_id.insert(id.to_string(), keys);
        }
    }

    /// Returns true when `data` matches any key bound to `id`.
    pub fn matches(&self, data: &str, id: &str) -> bool {
        self.keys_by_id.get(id).is_some_and(|keys| keys.iter().any(|key| matches_key(data, key)))
    }

    /// The keys bound to `id`; empty for unknown or disabled ids.
    pub fn get_keys(&self, id: &str) -> Vec<String> {
        self.keys_by_id.get(id).cloned().unwrap_or_default()
    }

    /// The definition for `id`, if any.
    pub fn get_definition(&self, id: &str) -> Option<&KeybindingDefinition> {
        self.definitions.get(id)
    }

    /// All definitions, in declaration order.
    pub fn get_definitions(&self) -> &KeybindingDefinitions {
        &self.definitions
    }

    /// Keys that the user assigned to more than one known id.
    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    /// Replaces the user overrides and recomputes keys and conflicts.
    pub fn set_user_bindings(&mut self, user_bindings: KeybindingsConfig) {
        self.user_bindings = user_bindings;
        self.rebuild();
    }

    /// A copy of the user overrides.
    pub fn get_user_bindings(&self) -> KeybindingsConfig {
        self.user_bindings.clone()
    }

    /// The effective keys for every defined id, in declaration order. Ids with exactly one key
    /// map to a single string; others map to a list.
    pub fn get_resolved_bindings(&self) -> KeybindingsConfig {
        self.definitions
            .ids()
            .map(|id| {
                let mut keys = self.get_keys(id);
                let value =
                    if keys.len() == 1 { KeybindingValue::One(keys.remove(0)) } else { KeybindingValue::Many(keys) };
                (id, value)
            })
            .collect()
    }
}

static GLOBAL_KEYBINDINGS: RwLock<Option<Arc<KeybindingsManager>>> = RwLock::new(None);

/// The process-wide keybindings. Creates a manager with the TUI defaults on first use.
pub fn get_keybindings() -> Arc<KeybindingsManager> {
    if let Some(manager) = GLOBAL_KEYBINDINGS.read().unwrap_or_else(PoisonError::into_inner).as_ref() {
        return Arc::clone(manager);
    }
    let mut slot = GLOBAL_KEYBINDINGS.write().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(slot.get_or_insert_with(|| Arc::new(KeybindingsManager::default())))
}

/// Replaces the process-wide keybindings.
pub fn set_keybindings(manager: KeybindingsManager) {
    *GLOBAL_KEYBINDINGS.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(manager));
}

/// The selection index after a `tui.select.pageUp` or `tui.select.pageDown` key, moved by `page_size` and clamped to
/// `0..item_count - 1`. `None` when `data` is not a page key; `Some(0)` on an empty list, so callers check for items.
pub fn get_select_page_index(
    keybindings: &KeybindingsManager,
    data: &str,
    selected_index: usize,
    item_count: usize,
    page_size: usize,
) -> Option<usize> {
    let moved = if keybindings.matches(data, ids::SELECT_PAGE_UP) {
        selected_index.saturating_sub(page_size)
    } else if keybindings.matches(data, ids::SELECT_PAGE_DOWN) {
        selected_index.saturating_add(page_size)
    } else {
        return None;
    };
    Some(moved.min(item_count.saturating_sub(1)))
}

/// Edits a copy of the process-wide keybindings and installs it (for example to reload user
/// overrides, which upstream does by mutating the shared manager). Returns the new manager.
pub fn update_keybindings(edit: impl FnOnce(&mut KeybindingsManager)) -> Arc<KeybindingsManager> {
    let mut slot = GLOBAL_KEYBINDINGS.write().unwrap_or_else(PoisonError::into_inner);
    let mut manager = slot.as_deref().cloned().unwrap_or_default();
    edit(&mut manager);
    let manager = Arc::new(manager);
    *slot = Some(Arc::clone(&manager));
    manager
}
