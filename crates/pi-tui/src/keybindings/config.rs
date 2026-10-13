//! User keybinding overrides in the `keybindings.json` shape: `{ "id": "key" | ["key", ...] }`.

use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One override value: a single key, or a list of keys. An empty list disables the binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeybindingValue {
    /// A single key id, such as `"ctrl+x"`.
    One(String),
    /// Several key ids; empty disables the binding.
    Many(Vec<String>),
}

impl KeybindingValue {
    /// The key ids in this value, in order.
    pub fn keys(&self) -> &[String] {
        match self {
            Self::One(key) => std::slice::from_ref(key),
            Self::Many(keys) => keys,
        }
    }
}

impl From<&str> for KeybindingValue {
    fn from(key: &str) -> Self {
        Self::One(key.to_string())
    }
}

impl From<String> for KeybindingValue {
    fn from(key: String) -> Self {
        Self::One(key)
    }
}

impl From<Vec<String>> for KeybindingValue {
    fn from(keys: Vec<String>) -> Self {
        Self::Many(keys)
    }
}

impl From<Vec<&str>> for KeybindingValue {
    fn from(keys: Vec<&str>) -> Self {
        Self::Many(keys.into_iter().map(str::to_string).collect())
    }
}

impl<const N: usize> From<[&str; N]> for KeybindingValue {
    fn from(keys: [&str; N]) -> Self {
        Self::Many(keys.into_iter().map(str::to_string).collect())
    }
}

/// Keybinding overrides by id, in insertion order (the JSON object order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingsConfig {
    entries: Vec<(String, KeybindingValue)>,
}

impl KeybindingsConfig {
    /// Creates an empty config.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the value for `id`. An existing id keeps its position, like a JavaScript object.
    pub fn insert(&mut self, id: impl Into<String>, value: impl Into<KeybindingValue>) -> Option<KeybindingValue> {
        let id = id.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(existing, _)| *existing == id) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.entries.push((id, value));
                None
            }
        }
    }

    /// The value for `id`, if set.
    pub fn get(&self, id: &str) -> Option<&KeybindingValue> {
        self.entries.iter().find(|(existing, _)| existing == id).map(|(_, value)| value)
    }

    /// Removes `id` and returns its value.
    pub fn remove(&mut self, id: &str) -> Option<KeybindingValue> {
        let index = self.entries.iter().position(|(existing, _)| existing == id)?;
        Some(self.entries.remove(index).1)
    }

    /// Returns true when `id` has a value.
    pub fn contains_key(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Number of ids with a value.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when no id has a value.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates over `(id, value)` pairs in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &KeybindingValue)> {
        self.entries.iter().map(|(id, value)| (id.as_str(), value))
    }
}

impl<K: Into<String>, V: Into<KeybindingValue>> FromIterator<(K, V)> for KeybindingsConfig {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut config = Self::new();
        config.extend(iter);
        config
    }
}

impl<K: Into<String>, V: Into<KeybindingValue>> Extend<(K, V)> for KeybindingsConfig {
    fn extend<I: IntoIterator<Item = (K, V)>>(&mut self, iter: I) {
        for (id, value) in iter {
            self.insert(id, value);
        }
    }
}

impl IntoIterator for KeybindingsConfig {
    type Item = (String, KeybindingValue);
    type IntoIter = std::vec::IntoIter<(String, KeybindingValue)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl Serialize for KeybindingsConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (id, value) in &self.entries {
            map.serialize_entry(id, value)?;
        }
        map.end()
    }
}

struct ConfigVisitor;

impl<'de> Visitor<'de> for ConfigVisitor {
    type Value = KeybindingsConfig;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object mapping keybinding ids to a key or a list of keys")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut config = KeybindingsConfig::new();
        while let Some((id, value)) = access.next_entry::<String, KeybindingValue>()? {
            config.insert(id, value);
        }
        Ok(config)
    }
}

impl<'de> Deserialize<'de> for KeybindingsConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(ConfigVisitor)
    }
}
