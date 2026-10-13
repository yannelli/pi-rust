//! Strict-JSON validation and copying at adapter boundaries (upstream `json.ts`).

use serde::Serialize;

use crate::error::{ChordError, ChordResult};
use crate::json::Json;
use crate::json::ser::{JsonSerializer, NoneMode};

/// Options for [`copy_json`].
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyJsonOptions {
    /// Omit object properties whose value is an absent `Option`; by default they are rejected.
    pub omit_undefined_properties: bool,
}

/// Copy a serializable value into an owned strict-JSON tree, rejecting non-finite numbers.
pub fn copy_json<T: ?Sized + Serialize>(value: &T, options: CopyJsonOptions) -> ChordResult<Json> {
    let none = if options.omit_undefined_properties { NoneMode::Omit } else { NoneMode::Reject };
    let serializer = JsonSerializer { none };
    match value.serialize(serializer)? {
        Some(json) => Ok(json),
        None => Err(ChordError::type_error("Value contains a non-JSON undefined; expected strict JSON")),
    }
}

/// Whether a serializable value is finite strict JSON.
pub fn is_json_value<T: ?Sized + Serialize>(value: &T) -> bool {
    copy_json(value, CopyJsonOptions::default()).is_ok()
}

impl Json {
    /// Serialize a Rust value into strict JSON. Absent options become `null`.
    pub fn from_serialize<T: ?Sized + Serialize>(value: &T) -> ChordResult<Json> {
        match value.serialize(JsonSerializer { none: NoneMode::Null })? {
            Some(json) => Ok(json),
            None => Ok(Json::Null),
        }
    }
}
