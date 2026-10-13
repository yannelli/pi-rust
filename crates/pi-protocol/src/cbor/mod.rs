//! The protocol's strict, definite-length RFC 8949 CBOR subset: JavaScript-shaped values with
//! safe integers, finite doubles, UTF-8 text, byte strings, arrays, and string-keyed maps.

mod decoder;
mod encoder;

use pi_chord::{Json, JsonMap};

pub use decoder::{decode_cbor, decode_json_cbor};
pub use encoder::{encode_cbor, encode_json_cbor};

/// Default maximum encoded bytes, and maximum byte or text string length.
pub const DEFAULT_MAX_CBOR_BYTE_LENGTH: u32 = 16 * 1024 * 1024;
/// Default maximum number of array elements or map entries.
pub const DEFAULT_MAX_CBOR_CONTAINER_LENGTH: u32 = 1_000_000;
/// Default maximum recursive item depth.
pub const DEFAULT_MAX_CBOR_DEPTH: u32 = 64;
const MAX_CONFIGURED_DEPTH: u32 = 512;

pub(crate) const UINT32_BASE: u64 = 0x1_0000_0000;
pub(crate) const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Malformed, unsupported, or over-limit CBOR.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CborError(pub String);

impl CborError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Limits for untrusted payloads. `None` selects the default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CborOptions {
    /// Maximum encoded input or output bytes and maximum byte or text string length.
    pub max_byte_length: Option<u32>,
    /// Maximum number of elements in an array or entries in a map.
    pub max_container_length: Option<u32>,
    /// Maximum recursive item depth, at most 512.
    pub max_depth: Option<u32>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Resolved {
    pub max_byte_length: usize,
    pub max_container_length: usize,
    pub max_depth: usize,
}

impl CborOptions {
    pub(crate) fn resolve(&self) -> Result<Resolved, CborError> {
        let max_depth = self.max_depth.unwrap_or(DEFAULT_MAX_CBOR_DEPTH);
        if max_depth > MAX_CONFIGURED_DEPTH {
            return Err(CborError::new(format!("maxDepth must be an integer between 0 and {MAX_CONFIGURED_DEPTH}")));
        }
        Ok(Resolved {
            max_byte_length: self.max_byte_length.unwrap_or(DEFAULT_MAX_CBOR_BYTE_LENGTH) as usize,
            max_container_length: self.max_container_length.unwrap_or(DEFAULT_MAX_CBOR_CONTAINER_LENGTH) as usize,
            max_depth: max_depth as usize,
        })
    }
}

/// One decoded or encodable CBOR item in the protocol subset.
#[derive(Clone, Debug, PartialEq)]
pub enum CborValue {
    Null,
    Bool(bool),
    /// A JavaScript number. Safe integers other than `-0` encode as CBOR integers.
    Number(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<CborValue>),
    /// String-keyed entries in encoding order.
    Map(Vec<(String, CborValue)>),
}

impl CborValue {
    /// The strict JSON form, or `None` when the value contains a byte string.
    pub fn to_json(&self) -> Option<Json> {
        Some(match self {
            CborValue::Null => Json::Null,
            CborValue::Bool(value) => Json::Bool(*value),
            CborValue::Number(value) => Json::from_f64(*value)?,
            CborValue::Bytes(_) => return None,
            CborValue::Text(value) => Json::from(value.as_str()),
            CborValue::Array(items) => Json::array(items.iter().map(CborValue::to_json).collect::<Option<_>>()?),
            CborValue::Map(entries) => {
                let mut map = JsonMap::new();
                for (key, value) in entries {
                    map.insert(key.as_str(), value.to_json()?);
                }
                Json::object(map)
            }
        })
    }
}

impl From<&Json> for CborValue {
    fn from(value: &Json) -> Self {
        match value {
            Json::Null => CborValue::Null,
            Json::Bool(value) => CborValue::Bool(*value),
            Json::Number(number) => CborValue::Number(number.as_f64()),
            Json::String(text) => CborValue::Text(text.to_string()),
            Json::Array(items) => CborValue::Array(items.iter().map(CborValue::from).collect()),
            Json::Object(map) => {
                CborValue::Map(map.iter().map(|(key, value)| (key.to_string(), CborValue::from(value))).collect())
            }
        }
    }
}

impl From<Json> for CborValue {
    fn from(value: Json) -> Self {
        CborValue::from(&value)
    }
}
