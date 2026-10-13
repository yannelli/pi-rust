//! Attribute values and attribute maps (upstream `AttributeValue` and `SpanAttributes`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A scalar or homogeneous array attribute value. Serializes as plain JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttributeValue {
    String(String),
    Number(f64),
    Boolean(bool),
    StringArray(Vec<String>),
    NumberArray(Vec<f64>),
    BooleanArray(Vec<bool>),
}

impl AttributeValue {
    /// The schema type name of this value (`string`, `number[]`, ...).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::String(_) => "string",
            Self::Number(_) => "number",
            Self::Boolean(_) => "boolean",
            Self::StringArray(_) => "string[]",
            Self::NumberArray(_) => "number[]",
            Self::BooleanArray(_) => "boolean[]",
        }
    }
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

macro_rules! number_from {
    ($($ty:ty),*) => {$(
        impl From<$ty> for AttributeValue {
            fn from(value: $ty) -> Self {
                Self::Number(value as f64)
            }
        }
    )*};
}
number_from!(f64, f32, i64, i32, u64, u32, usize);

impl From<Vec<String>> for AttributeValue {
    fn from(value: Vec<String>) -> Self {
        Self::StringArray(value)
    }
}

impl From<Vec<&str>> for AttributeValue {
    fn from(value: Vec<&str>) -> Self {
        Self::StringArray(value.into_iter().map(String::from).collect())
    }
}

impl From<Vec<f64>> for AttributeValue {
    fn from(value: Vec<f64>) -> Self {
        Self::NumberArray(value)
    }
}

impl From<Vec<bool>> for AttributeValue {
    fn from(value: Vec<bool>) -> Self {
        Self::BooleanArray(value)
    }
}

/// Named attribute values. Upstream drops `undefined` entries; here an absent value
/// (`None` passed to [`SpanAttributes::with_optional`]) is never stored.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpanAttributes(BTreeMap<String, AttributeValue>);

impl SpanAttributes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace one attribute.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<AttributeValue>) -> Self {
        self.set(name, value);
        self
    }

    /// Add one attribute when `value` is present.
    pub fn with_optional<V: Into<AttributeValue>>(mut self, name: impl Into<String>, value: Option<V>) -> Self {
        if let Some(value) = value {
            self.set(name, value);
        }
        self
    }

    pub fn set(&mut self, name: impl Into<String>, value: impl Into<AttributeValue>) {
        self.0.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.0.get(name)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &AttributeValue)> {
        self.0.iter()
    }

    /// Overwrite entries with those of `other`, keeping the rest.
    pub fn merge(&mut self, other: &SpanAttributes) {
        for (name, value) in other.iter() {
            self.0.insert(name.clone(), value.clone());
        }
    }
}

impl<K: Into<String>, V: Into<AttributeValue>> FromIterator<(K, V)> for SpanAttributes {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self(iter.into_iter().map(|(name, value)| (name.into(), value.into())).collect())
    }
}

impl<K: Into<String>, V: Into<AttributeValue>, const N: usize> From<[(K, V); N]> for SpanAttributes {
    fn from(entries: [(K, V); N]) -> Self {
        entries.into_iter().collect()
    }
}
