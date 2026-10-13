//! Conversions between [`Json`], Rust primitives, `serde_json`, and serde.

use std::fmt;
use std::sync::Arc;

use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{Json, JsonMap, Number};
use crate::error::{ChordError, ChordResult};

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Self::String(value.into())
    }
}

impl From<Arc<str>> for Json {
    fn from(value: Arc<str>) -> Self {
        Self::String(value)
    }
}

impl From<Number> for Json {
    fn from(value: Number) -> Self {
        Self::Number(value)
    }
}

macro_rules! json_from_integer {
    ($($ty:ty),*) => {$(
        impl From<$ty> for Json {
            fn from(value: $ty) -> Self {
                Self::Number(Number::from(value))
            }
        }
    )*};
}
json_from_integer!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

/// Non-finite floats become `null`, as `JSON.stringify` writes them.
impl From<f64> for Json {
    fn from(value: f64) -> Self {
        Number::new(value).map_or(Json::Null, Json::Number)
    }
}

static NULL: Json = Json::Null;

/// Read-only indexing like `serde_json`: a missing key or index yields `null`.
impl std::ops::Index<&str> for Json {
    type Output = Json;
    fn index(&self, key: &str) -> &Json {
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<usize> for Json {
    type Output = Json;
    fn index(&self, index: usize) -> &Json {
        self.at(index).unwrap_or(&NULL)
    }
}

impl From<Vec<Json>> for Json {
    fn from(value: Vec<Json>) -> Self {
        Self::array(value)
    }
}

impl From<JsonMap> for Json {
    fn from(value: JsonMap) -> Self {
        Self::object(value)
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(value: Option<T>) -> Self {
        value.map_or(Json::Null, Into::into)
    }
}

impl From<serde_json::Value> for Json {
    fn from(value: serde_json::Value) -> Self {
        Json::from(&value)
    }
}

impl From<&serde_json::Value> for Json {
    fn from(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Json::Null,
            serde_json::Value::Bool(value) => Json::Bool(*value),
            serde_json::Value::Number(number) => number.as_f64().and_then(Json::from_f64).unwrap_or(Json::Null),
            serde_json::Value::String(text) => Json::from(text.as_str()),
            serde_json::Value::Array(items) => Json::array(items.iter().map(Json::from).collect()),
            serde_json::Value::Object(map) => {
                Json::object(map.iter().map(|(key, value)| (key.as_str(), Json::from(value))).collect())
            }
        }
    }
}

impl From<&Json> for serde_json::Value {
    fn from(value: &Json) -> Self {
        value.to_serde()
    }
}

impl From<Json> for serde_json::Value {
    fn from(value: Json) -> Self {
        value.to_serde()
    }
}

impl Json {
    /// Convert to a `serde_json` value. Integral numbers become integers.
    pub fn to_serde(&self) -> serde_json::Value {
        match self {
            Json::Null => serde_json::Value::Null,
            Json::Bool(value) => serde_json::Value::Bool(*value),
            Json::Number(number) => match number.as_i64() {
                Some(integer) => serde_json::Value::from(integer),
                None => serde_json::Number::from_f64(number.as_f64()).map_or(serde_json::Value::Null, Into::into),
            },
            Json::String(text) => serde_json::Value::String(text.to_string()),
            Json::Array(items) => serde_json::Value::Array(items.iter().map(Json::to_serde).collect()),
            Json::Object(map) => {
                serde_json::Value::Object(map.iter().map(|(key, value)| (key.to_string(), value.to_serde())).collect())
            }
        }
    }

    /// Parse JSON text.
    pub fn parse(text: &str) -> ChordResult<Json> {
        serde_json::from_str(text).map_err(|error| ChordError::type_error(format!("Invalid JSON: {error}")))
    }

    /// Deserialize this value into a Rust type.
    pub fn deserialize_into<T: DeserializeOwned>(&self) -> ChordResult<T> {
        serde_json::from_value(self.to_serde()).map_err(|error| ChordError::type_error(error.to_string()))
    }
}

impl Serialize for Json {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Json::Null => serializer.serialize_unit(),
            Json::Bool(value) => serializer.serialize_bool(*value),
            Json::Number(number) => {
                let value = number.as_f64();
                if value.fract() == 0.0 && value.abs() < 1e21 {
                    if let Some(integer) = number.as_i64() {
                        return serializer.serialize_i64(integer);
                    }
                    return serializer.serialize_i128(value as i128);
                }
                serializer.serialize_f64(value)
            }
            Json::String(text) => serializer.serialize_str(text),
            Json::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items.iter() {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Json::Object(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                for (key, value) in map.iter() {
                    out.serialize_entry(&**key, value)?;
                }
                out.end()
            }
        }
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a strict JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Json, D::Error> {
        Json::deserialize(deserializer)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Json, E> {
        Ok(Json::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Json, E> {
        Ok(Json::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Json, E> {
        Json::from_f64(value).ok_or_else(|| E::custom("non-finite number is not strict JSON"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Json, E> {
        Ok(Json::from(value))
    }

    fn visit_string<E>(self, value: String) -> Result<Json, E> {
        Ok(Json::from(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
        while let Some(item) = seq.next_element::<Json>()? {
            items.push(item);
        }
        Ok(Json::array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Json, A::Error> {
        let mut map = JsonMap::new();
        while let Some((key, value)) = access.next_entry::<String, Json>()? {
            map.insert(key, value);
        }
        Ok(Json::object(map))
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Json, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

impl std::str::FromStr for Json {
    type Err = ChordError;

    fn from_str(text: &str) -> ChordResult<Json> {
        Json::parse(text)
    }
}

/// Build a [`Json`] value with `serde_json::json!` syntax.
#[macro_export]
macro_rules! json {
    ($($tokens:tt)+) => {
        $crate::Json::from($crate::__private::serde_json::json!($($tokens)+))
    };
}
