//! JSON objects with JavaScript property order: array-index keys ascending, then insertion order.

use std::sync::Arc;

use indexmap::IndexMap;

use super::Json;

/// An ordered JSON object. Canonical array-index keys sort numerically before other keys,
/// as JavaScript orders own properties, so serialization matches `JSON.stringify`.
#[derive(Clone, Default)]
pub struct JsonMap {
    entries: IndexMap<Arc<str>, Json>,
    index_keys: usize,
}

/// The numeric value of a canonical array-index key such as `"0"` or `"42"`.
fn array_index_key(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>().ok().filter(|value| *value < u64::from(u32::MAX)).map(|value| value as u32)
}

impl JsonMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.entries.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.entries.get_mut(key)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Set a property. An existing key keeps its position, like a JavaScript assignment.
    pub fn insert(&mut self, key: impl Into<Arc<str>>, value: Json) -> Option<Json> {
        let key = key.into();
        if let Some(slot) = self.entries.get_mut(&*key) {
            return Some(std::mem::replace(slot, value));
        }
        match array_index_key(&key) {
            Some(index) => {
                let position = self
                    .entries
                    .keys()
                    .take(self.index_keys)
                    .position(|existing| array_index_key(existing).is_some_and(|existing| existing > index));
                let position = position.unwrap_or(self.index_keys);
                self.entries.shift_insert(position, key, value);
                self.index_keys += 1;
            }
            None => {
                self.entries.insert(key, value);
            }
        }
        None
    }

    /// Delete a property, keeping the order of the others.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let removed = self.entries.shift_remove(key)?;
        if array_index_key(key).is_some() {
            self.index_keys -= 1;
        }
        Some(removed)
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &Arc<str>> {
        self.entries.keys()
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = &Json> {
        self.entries.values()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&Arc<str>, &Json)> {
        self.entries.iter()
    }

    pub fn iter_mut(&mut self) -> impl ExactSizeIterator<Item = (&Arc<str>, &mut Json)> {
        self.entries.iter_mut()
    }
}

impl<K: Into<Arc<str>>> FromIterator<(K, Json)> for JsonMap {
    fn from_iter<I: IntoIterator<Item = (K, Json)>>(iter: I) -> Self {
        let mut map = JsonMap::new();
        for (key, value) in iter {
            map.insert(key, value);
        }
        map
    }
}

impl<'a> IntoIterator for &'a JsonMap {
    type Item = (&'a Arc<str>, &'a Json);
    type IntoIter = indexmap::map::Iter<'a, Arc<str>, Json>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

impl PartialEq for JsonMap {
    /// Deep equality that ignores key order, like upstream `equalJson`.
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().all(|(key, value)| other.get(key).is_some_and(|o| o == value))
    }
}

impl std::fmt::Debug for JsonMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_index_keys_first_like_javascript() {
        let mut map = JsonMap::new();
        for key in ["b", "10", "a", "2", "01", "4294967295", "0"] {
            map.insert(key, Json::Null);
        }
        let keys: Vec<&str> = map.keys().map(|key| &**key).collect();
        assert_eq!(keys, ["0", "2", "10", "b", "a", "01", "4294967295"]);
        map.remove("2");
        map.insert("1", Json::Null);
        let keys: Vec<&str> = map.keys().map(|key| &**key).collect();
        assert_eq!(keys, ["0", "1", "10", "b", "a", "01", "4294967295"]);
    }
}
