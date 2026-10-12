//! `store()` persistence: each successful script that stores values appends one `codemode-store`
//! custom entry, so every branch sees only the values written on its path.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::sandbox::CodemodeStoreWrites;

/// Custom entry type holding one script's `store()` writes as [`CodemodeStoreEntryData`].
pub const CODEMODE_STORE_ENTRY_TYPE: &str = "codemode-store";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreEntryData {
    pub set: Map<String, Value>,
    pub delete: Vec<String>,
}

impl From<CodemodeStoreWrites> for CodemodeStoreEntryData {
    fn from(writes: CodemodeStoreWrites) -> Self {
        Self { set: writes.set, delete: writes.delete }
    }
}

impl CodemodeStoreEntryData {
    /// Decode entry data, or `None` when it does not have the `codemode-store` shape.
    pub fn from_value(data: &Value) -> Option<Self> {
        let set = data.get("set")?.as_object()?.clone();
        let delete = data.get("delete")?.as_array()?;
        let delete = delete.iter().map(|key| key.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?;
        Some(Self { set, delete })
    }
}

/// Values of `load()`: the `codemode-store` entries of a branch, applied from the root. Takes the
/// custom entries as `(custom type, data)`; other types and malformed data are skipped.
pub fn read_codemode_store<'a>(entries: impl IntoIterator<Item = (&'a str, &'a Value)>) -> Map<String, Value> {
    let mut store = Map::new();
    for (custom_type, data) in entries {
        if custom_type != CODEMODE_STORE_ENTRY_TYPE {
            continue;
        }
        let Some(entry) = CodemodeStoreEntryData::from_value(data) else { continue };
        for key in &entry.delete {
            store.shift_remove(key);
        }
        for (key, value) in entry.set {
            store.insert(key, value);
        }
    }
    store
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folds_store_entries_from_the_root_ignoring_malformed_data() {
        let entries = [
            (CODEMODE_STORE_ENTRY_TYPE, json!({ "set": { "a": 1, "b": { "c": 2 } }, "delete": [] })),
            (CODEMODE_STORE_ENTRY_TYPE, json!({ "set": { "a": 3 }, "delete": ["b"] })),
            (CODEMODE_STORE_ENTRY_TYPE, json!({ "set": { "z": 1 } })),
            ("other-extension", json!({ "set": { "other": 1 }, "delete": [] })),
        ];
        let store = read_codemode_store(entries.iter().map(|(kind, data)| (*kind, data)));
        assert_eq!(Value::Object(store), json!({ "a": 3 }));
    }

    #[test]
    fn serializes_entry_data_like_upstream() {
        let data = CodemodeStoreEntryData { set: json!({ "count": 1 }).as_object().cloned().unwrap(), delete: vec![] };
        assert_eq!(serde_json::to_value(&data).unwrap(), json!({ "set": { "count": 1 }, "delete": [] }));
    }
}
