//! Records, document content, and the Storage contract shared by every layer.

mod documents;
mod records;
mod storage;
mod tasks;

pub use documents::*;
pub use records::*;
pub use storage::*;
pub use tasks::*;

/// Serde adapter for an optional `JsonMap`, which has no serde implementation of its own.
pub(crate) mod optional_map {
    use pi_chord::{Json, JsonMap};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<JsonMap>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(map) => Json::object(map.clone()).serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<JsonMap>, D::Error> {
        match Option::<Json>::deserialize(deserializer)? {
            None | Some(Json::Null) => Ok(None),
            Some(Json::Object(map)) => Ok(Some((*map).clone())),
            Some(_) => Err(serde::de::Error::custom("expected a JSON object")),
        }
    }
}
