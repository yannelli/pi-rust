//! Immutable, structurally shared strict JSON. Containers sit behind `Arc`, so clones are O(1)
//! and unchanged subtrees are shared; mutation copies on write and never affects other holders.

mod convert;
mod map;
mod number;
pub(crate) mod ser;
mod text;

use std::sync::Arc;

pub use map::JsonMap;
pub(crate) use map::array_index_key;
pub use number::{Number, format_js_number};
pub(crate) use text::{utf16_len, utf16_offset, write_json_string};

use crate::error::{ChordError, ChordResult};

/// A strict JSON value: finite numbers, no undefined, dense arrays, and plain objects.
#[derive(Clone, Default)]
pub enum Json {
    #[default]
    Null,
    Bool(bool),
    Number(Number),
    String(Arc<str>),
    Array(Arc<Vec<Json>>),
    Object(Arc<JsonMap>),
}

/// Identity of a value under JavaScript `SameValueZero`: primitives by value, containers by pointer.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum Identity {
    Null,
    Bool(bool),
    Number(u64),
    String(Arc<str>),
    Container(usize),
}

impl Json {
    pub fn array(items: Vec<Json>) -> Self {
        Self::Array(Arc::new(items))
    }

    pub fn object(map: JsonMap) -> Self {
        Self::Object(Arc::new(map))
    }

    pub fn empty_object() -> Self {
        Self::object(JsonMap::new())
    }

    /// A finite number, or `None` for NaN and infinities.
    pub fn from_f64(value: f64) -> Option<Self> {
        Number::new(value).map(Self::Number)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn is_container(&self) -> bool {
        matches!(self, Self::Array(_) | Self::Object(_))
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(value.as_f64()),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Number(value) => value.as_i64(),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => value.as_u64(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&JsonMap> {
        match self {
            Self::Object(map) => Some(map),
            _ => None,
        }
    }

    /// An object property.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?.get(key)
    }

    /// An array element.
    pub fn at(&self, index: usize) -> Option<&Json> {
        self.as_array()?.get(index)
    }

    /// The array, copied first if another revision shares it.
    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Json>> {
        match self {
            Self::Array(items) => Some(Arc::make_mut(items)),
            _ => None,
        }
    }

    /// The object, copied first if another revision shares it.
    pub fn as_object_mut(&mut self) -> Option<&mut JsonMap> {
        match self {
            Self::Object(map) => Some(Arc::make_mut(map)),
            _ => None,
        }
    }

    /// A mutable object property, copying shared containers on the way.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        self.as_object_mut()?.get_mut(key)
    }

    /// A mutable array element, copying shared containers on the way.
    pub fn at_mut(&mut self, index: usize) -> Option<&mut Json> {
        self.as_array_mut()?.get_mut(index)
    }

    /// Set an object property. Fails when this is not an object.
    pub fn set(&mut self, key: impl Into<Arc<str>>, value: impl Into<Json>) -> ChordResult<()> {
        let map =
            self.as_object_mut().ok_or_else(|| ChordError::type_error("Cannot set a property on a non-object"))?;
        map.insert(key, value.into());
        Ok(())
    }

    /// Delete an object property, returning it when present.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        self.as_object_mut()?.remove(key)
    }

    /// Append an array element. Fails when this is not an array.
    pub fn push(&mut self, value: impl Into<Json>) -> ChordResult<()> {
        let items = self.as_array_mut().ok_or_else(|| ChordError::type_error("Cannot push onto a non-array"))?;
        items.push(value.into());
        Ok(())
    }

    /// Append text to a string. Fails when this is not a string.
    pub fn push_str(&mut self, text: &str) -> ChordResult<()> {
        match self {
            Self::String(current) => {
                let mut next = String::with_capacity(current.len() + text.len());
                next.push_str(current);
                next.push_str(text);
                *current = next.into();
                Ok(())
            }
            _ => Err(ChordError::type_error("Cannot append text to a non-string")),
        }
    }

    /// Resolve a path of object keys and array indices.
    pub fn pointer(&self, path: &[crate::delta::Seg]) -> Option<&Json> {
        let mut node = self;
        for segment in path {
            node = match segment {
                crate::delta::Seg::Key(key) => node.get(key)?,
                crate::delta::Seg::Index(index) => node.at(usize::try_from(*index).ok()?)?,
            };
        }
        Some(node)
    }

    /// Resolve a path mutably, copying every shared container along it.
    pub fn pointer_mut(&mut self, path: &[crate::delta::Seg]) -> Option<&mut Json> {
        let mut node = self;
        for segment in path {
            node = match segment {
                crate::delta::Seg::Key(key) => node.get_mut(key)?,
                crate::delta::Seg::Index(index) => node.at_mut(usize::try_from(*index).ok()?)?,
            };
        }
        Some(node)
    }

    /// JavaScript `===` on values: primitives compare by value, containers by identity.
    pub fn same_ref(&self, other: &Json) -> bool {
        match (self, other) {
            (Self::Array(left), Self::Array(right)) => Arc::ptr_eq(left, right),
            (Self::Object(left), Self::Object(right)) => Arc::ptr_eq(left, right),
            (Self::Array(_) | Self::Object(_), _) | (_, Self::Array(_) | Self::Object(_)) => false,
            _ => self == other,
        }
    }

    pub(crate) fn identity(&self) -> Identity {
        match self {
            Self::Null => Identity::Null,
            Self::Bool(value) => Identity::Bool(*value),
            Self::Number(value) => {
                let value = value.as_f64();
                Identity::Number(if value == 0.0 { 0 } else { value.to_bits() })
            }
            Self::String(value) => Identity::String(value.clone()),
            Self::Array(items) => Identity::Container(Arc::as_ptr(items) as usize),
            Self::Object(map) => Identity::Container(Arc::as_ptr(map) as usize),
        }
    }

    /// A short type name used in error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
        }
    }
}

impl PartialEq for Json {
    /// Deep equality; object key order is ignored, like upstream `equalJson`.
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => Arc::ptr_eq(left, right) || left == right,
            (Self::Object(left), Self::Object(right)) => Arc::ptr_eq(left, right) || left == right,
            _ => false,
        }
    }
}

impl std::fmt::Debug for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
