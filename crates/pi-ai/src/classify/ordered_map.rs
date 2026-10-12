//! A string-keyed map that keeps insertion order and serializes as a JSON object.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Entries keep insertion order, like the string keys of a JavaScript object.
/// Inserting a key that exists replaces its value in place.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderedMap<V>(Vec<(String, V)>);

impl<V> Default for OrderedMap<V> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<V> OrderedMap<V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or replace `key`, returning the replaced value.
    pub fn insert(&mut self, key: impl Into<String>, value: V) -> Option<V> {
        let key = key.into();
        match self.0.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, slot)) => Some(std::mem::replace(slot, value)),
            None => {
                self.0.push((key, value));
                None
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.0.iter().find(|(existing, _)| existing == key).map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(key, _)| key.as_str())
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.0.iter().map(|(_, value)| value)
    }
}

impl<V> std::ops::Index<&str> for OrderedMap<V> {
    type Output = V;

    fn index(&self, key: &str) -> &V {
        self.get(key).unwrap_or_else(|| panic!("no entry for key {key:?}"))
    }
}

impl<K: Into<String>, V> FromIterator<(K, V)> for OrderedMap<V> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(entries: I) -> Self {
        let mut map = Self::new();
        for (key, value) in entries {
            map.insert(key, value);
        }
        map
    }
}

impl<V> IntoIterator for OrderedMap<V> {
    type Item = (String, V);
    type IntoIter = std::vec::IntoIter<(String, V)>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<V: Serialize> Serialize for OrderedMap<V> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for OrderedMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(OrderedMapVisitor(PhantomData))
    }
}

struct OrderedMapVisitor<V>(PhantomData<V>);

impl<'de, V: Deserialize<'de>> Visitor<'de> for OrderedMapVisitor<V> {
    type Value = OrderedMap<V>;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut map = OrderedMap::new();
        while let Some((key, value)) = access.next_entry::<String, V>()? {
            map.insert(key, value);
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_insertion_order_and_replaces_in_place() {
        let mut map: OrderedMap<i32> = serde_json::from_str(r#"{"b":1,"a":2,"__proto__":3}"#).unwrap();
        assert_eq!(map.keys().collect::<Vec<_>>(), ["b", "a", "__proto__"]);
        assert_eq!(map.insert("b", 4), Some(1));
        assert_eq!(serde_json::to_string(&map).unwrap(), r#"{"b":4,"a":2,"__proto__":3}"#);
        assert_eq!(map["__proto__"], 3);
    }
}
