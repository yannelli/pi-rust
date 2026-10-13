//! JSON-RPC 2.0 messages.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value, json};

use crate::error::McpError;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Number(Number),
    String(String),
}

impl JsonRpcId {
    fn parse(value: &Value) -> Option<Self> {
        match value {
            Value::String(text) => Some(Self::String(text.clone())),
            Value::Number(number) => Some(Self::Number(number.clone())),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Self::Number(number) => Value::Number(number.clone()),
            Self::String(text) => Value::String(text.clone()),
        }
    }
}

impl From<i64> for JsonRpcId {
    fn from(value: i64) -> Self {
        Self::Number(value.into())
    }
}

impl From<&str> for JsonRpcId {
    fn from(value: &str) -> Self {
        Self::String(value.into())
    }
}

impl fmt::Display for JsonRpcId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(formatter, "{number}"),
            Self::String(text) => formatter.write_str(text),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl From<JsonRpcErrorObject> for McpError {
    fn from(error: JsonRpcErrorObject) -> Self {
        McpError::Rpc { code: error.code, message: error.message, data: error.data }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum JsonRpcMessage {
    Request { id: JsonRpcId, method: String, params: Option<Value> },
    Notification { method: String, params: Option<Value> },
    Response { id: JsonRpcId, result: Result<Value, JsonRpcErrorObject> },
}

impl JsonRpcMessage {
    pub fn request(id: impl Into<JsonRpcId>, method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Request { id: id.into(), method: method.into(), params }
    }

    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::Notification { method: method.into(), params }
    }

    pub fn result(id: JsonRpcId, result: Value) -> Self {
        Self::Response { id, result: Ok(result) }
    }

    pub fn error(id: JsonRpcId, code: i64, message: impl Into<String>) -> Self {
        Self::Response { id, result: Err(JsonRpcErrorObject { code, message: message.into(), data: None }) }
    }

    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } => None,
        }
    }

    pub fn id(&self) -> Option<&JsonRpcId> {
        match self {
            Self::Request { id, .. } | Self::Response { id, .. } => Some(id),
            Self::Notification { .. } => None,
        }
    }

    pub fn is_request(&self) -> bool {
        matches!(self, Self::Request { .. })
    }

    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        object.insert("jsonrpc".into(), json!("2.0"));
        match self {
            Self::Request { id, method, params } => {
                object.insert("id".into(), id.to_value());
                object.insert("method".into(), json!(method));
                if let Some(params) = params {
                    object.insert("params".into(), params.clone());
                }
            }
            Self::Notification { method, params } => {
                object.insert("method".into(), json!(method));
                if let Some(params) = params {
                    object.insert("params".into(), params.clone());
                }
            }
            Self::Response { id, result } => {
                object.insert("id".into(), id.to_value());
                match result {
                    Ok(result) => object.insert("result".into(), result.clone()),
                    Err(error) => object.insert("error".into(), json!(error)),
                };
            }
        }
        Value::Object(object)
    }

    /// Validate a decoded JSON value as a request, notification, or response.
    pub fn parse(value: &Value) -> Result<Self, McpError> {
        let invalid = || McpError::invalid("Invalid JSON-RPC message");
        let object =
            value.as_object().filter(|object| object.get("jsonrpc") == Some(&json!("2.0"))).ok_or_else(invalid)?;
        let id = object.get("id");
        let method = object.get("method").and_then(Value::as_str);
        let params = object.get("params").cloned();
        match (id, method) {
            (Some(id), Some(method)) => {
                let id = JsonRpcId::parse(id).ok_or_else(invalid)?;
                Ok(Self::Request { id, method: method.into(), params })
            }
            (None, Some(method)) => Ok(Self::Notification { method: method.into(), params }),
            (Some(id), None) => {
                let id = JsonRpcId::parse(id).ok_or_else(invalid)?;
                match (object.get("result"), object.get("error")) {
                    (Some(result), None) => Ok(Self::Response { id, result: Ok(result.clone()) }),
                    (None, Some(error)) => {
                        let error: JsonRpcErrorObject = serde_json::from_value(error.clone()).map_err(|_| invalid())?;
                        Ok(Self::Response { id, result: Err(error) })
                    }
                    _ => Err(invalid()),
                }
            }
            (None, None) => Err(invalid()),
        }
    }

    /// Parse one message from JSON text.
    pub fn from_json(text: &str) -> Result<Self, McpError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| McpError::rpc(crate::error::codes::PARSE_ERROR, error.to_string()))?;
        Self::parse(&value)
    }

    pub fn to_json(&self) -> String {
        self.to_value().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_serializes_messages() {
        let request = JsonRpcMessage::from_json(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).unwrap();
        assert_eq!(request, JsonRpcMessage::request(1, "ping", None));
        assert_eq!(request.to_json(), r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
        let error = JsonRpcMessage::from_json(r#"{"jsonrpc":"2.0","id":"a","error":{"code":-1,"message":"x"}}"#);
        assert!(matches!(error, Ok(JsonRpcMessage::Response { result: Err(_), .. })));
        for invalid in [
            r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":true,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{"code":1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":"1","message":"x"}}"#,
            r#"[]"#,
        ] {
            assert!(JsonRpcMessage::from_json(invalid).is_err(), "{invalid}");
        }
    }
}
