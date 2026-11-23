//! ACP wire types adapted from `agent-client-protocol-schema` 0.13.7 (see `LICENSE-agent-client-protocol-schema`).
//! Fields follow the serialization order of the pinned schema.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

mod capabilities;
mod config;
mod content;
mod error;
mod initialize;
mod mcp;
mod permission;
mod session;
mod tool_call;
mod update;

pub use capabilities::*;
pub use config::*;
pub use content::*;
pub use error::*;
pub use initialize::*;
pub use mcp::*;
pub use permission::*;
pub use session::*;
pub use tool_call::*;
pub use update::*;

/// Extension data of a protocol object (`_meta`).
pub type Meta = serde_json::Map<String, Value>;

/// A builder argument given as a value or as an optional value.
pub trait IntoOption<T> {
    fn into_option(self) -> Option<T>;
}

impl<T> IntoOption<T> for T {
    fn into_option(self) -> Option<T> {
        Some(self)
    }
}

impl<T> IntoOption<T> for Option<T> {
    fn into_option(self) -> Option<T> {
        self
    }
}

impl IntoOption<String> for &str {
    fn into_option(self) -> Option<String> {
        Some(self.to_string())
    }
}

/// A field that is absent, `null`, or a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum MaybeUndefined<T> {
    #[default]
    Undefined,
    Null,
    Value(T),
}

impl<T> MaybeUndefined<T> {
    pub const fn is_undefined(&self) -> bool {
        matches!(self, Self::Undefined)
    }
}

impl<T: Serialize> Serialize for MaybeUndefined<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Value(value) => value.serialize(serializer),
            Self::Null | Self::Undefined => serializer.serialize_none(),
        }
    }
}

/// A builder argument where `None` means `null`.
pub trait IntoMaybeUndefined<T> {
    fn into_maybe_undefined(self) -> MaybeUndefined<T>;
}

impl<T> IntoMaybeUndefined<T> for T {
    fn into_maybe_undefined(self) -> MaybeUndefined<T> {
        MaybeUndefined::Value(self)
    }
}

impl<T> IntoMaybeUndefined<T> for Option<T> {
    fn into_maybe_undefined(self) -> MaybeUndefined<T> {
        self.map_or(MaybeUndefined::Null, MaybeUndefined::Value)
    }
}

impl IntoMaybeUndefined<String> for &str {
    fn into_maybe_undefined(self) -> MaybeUndefined<String> {
        MaybeUndefined::Value(self.to_string())
    }
}

/// A value that does not parse becomes the default, as 0.13.7's `DefaultOnError` fields.
fn default_on_error<'de, D: Deserializer<'de>, T: DeserializeOwned + Default>(deserializer: D) -> Result<T, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(T::deserialize(value).unwrap_or_default())
}

/// An optional list that drops items that do not parse; anything but a list or `null` is `None`.
fn skip_invalid_items<'de, D: Deserializer<'de>, T: DeserializeOwned>(
    deserializer: D,
) -> Result<Option<Vec<T>>, D::Error> {
    Ok(match Value::deserialize(deserializer)? {
        Value::Array(items) => Some(items.into_iter().filter_map(|item| T::deserialize(item).ok()).collect()),
        _ => None,
    })
}

macro_rules! string_ids {
    ($($name:ident),* $(,)?) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_string())
            }
        }
    )*};
}

string_ids!(
    SessionId,
    SessionModeId,
    SessionConfigId,
    SessionConfigValueId,
    MessageId,
    ToolCallId,
    TerminalId,
    PermissionOptionId,
    AuthMethodId,
);
