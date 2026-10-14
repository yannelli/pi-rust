//! JSON conversion between typed service values and Chord's strict JSON.

use pi_chord::{ChordError, ChordResult, Json};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Encode a typed value as Chord JSON.
pub fn encode<T: Serialize>(value: &T) -> ChordResult<Json> {
    let value = serde_json::to_value(value).map_err(|error| ChordError::type_error(error.to_string()))?;
    Ok(Json::from(&value))
}

/// Decode argument `index` of a service call.
pub fn decode_arg<T: DeserializeOwned>(args: &[Json], index: usize) -> ChordResult<T> {
    let value = args.get(index).map_or(serde_json::Value::Null, Json::to_serde);
    serde_json::from_value(value).map_err(|error| ChordError::type_error(format!("Invalid argument {index}: {error}")))
}

/// Decode a service call result; a void result decodes as `null`.
pub fn decode_result<T: DeserializeOwned>(result: Option<Json>) -> ChordResult<T> {
    let value = result.as_ref().map_or(serde_json::Value::Null, Json::to_serde);
    serde_json::from_value(value).map_err(|error| ChordError::type_error(format!("Invalid service result: {error}")))
}
