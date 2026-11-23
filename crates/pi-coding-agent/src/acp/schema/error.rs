use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::IntoOption;

pub const INTERNAL_ERROR: i32 = -32603;

/// A JSON-RPC error object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Error {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Error {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn data(mut self, data: impl IntoOption<Value>) -> Self {
        self.data = data.into_option();
        self
    }

    pub fn internal_error() -> Self {
        Self::new(INTERNAL_ERROR, "Internal error")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.message.is_empty() {
            write!(formatter, "{}", self.code)?;
        } else {
            formatter.write_str(&self.message)?;
        }
        if let Some(data) = &self.data {
            let pretty = serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string());
            write!(formatter, ": {pretty}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}
