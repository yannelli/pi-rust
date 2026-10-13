//! Structural checks of server results, as lenient as the upstream client.

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::content::CallToolResult;
use crate::error::McpError;
use crate::types::{InitializeResult, ReadResourceResult, Resource, ResourceTemplate, Tool};

pub(crate) struct Page {
    pub items: Vec<Value>,
    pub next_cursor: Option<String>,
}

pub(crate) fn initialize_result(value: Value) -> Result<InitializeResult, McpError> {
    let invalid = || McpError::invalid("Invalid MCP initialize result");
    let object = value.as_object().ok_or_else(invalid)?;
    let info = object.get("serverInfo").and_then(Value::as_object);
    let valid = object.get("protocolVersion").is_some_and(Value::is_string)
        && object.get("capabilities").is_some_and(Value::is_object)
        && info.is_some_and(|info| {
            info.get("name").is_some_and(Value::is_string) && info.get("version").is_some_and(Value::is_string)
        })
        && object.get("instructions").is_none_or(Value::is_string);
    if !valid {
        return Err(invalid());
    }
    serde_json::from_value(value).map_err(|_| invalid())
}

pub(crate) fn list_page(method: &str, key: &str, value: Value, is_item: fn(&Value) -> bool) -> Result<Page, McpError> {
    let Value::Object(mut object) = value else {
        return Err(McpError::invalid(format!("Invalid MCP {method} result")));
    };
    let Some(Value::Array(items)) = object.remove(key) else {
        return Err(McpError::invalid(format!("Invalid MCP {method} result")));
    };
    if !items.iter().all(|item| item.is_object() && is_item(item)) {
        return Err(McpError::invalid(format!("Invalid entry in MCP {method} result")));
    }
    // Some servers end pagination with `null` or `""` instead of omitting the cursor.
    let next_cursor = match object.remove("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) if cursor.is_empty() => None,
        Some(Value::String(cursor)) => Some(cursor),
        Some(_) => return Err(McpError::invalid(format!("Invalid MCP {method} cursor"))),
    };
    Ok(Page { items, next_cursor })
}

fn optional_string(item: &Value, key: &str) -> bool {
    item.get(key).is_none_or(Value::is_string)
}

pub(crate) fn is_tool(item: &Value) -> bool {
    item.get("name").is_some_and(Value::is_string) && item.get("inputSchema").is_some_and(Value::is_object)
}

pub(crate) fn is_resource(item: &Value) -> bool {
    item.get("uri").is_some_and(Value::is_string) && optional_string(item, "name")
}

pub(crate) fn is_resource_template(item: &Value) -> bool {
    item.get("uriTemplate").is_some_and(Value::is_string) && optional_string(item, "name")
}

fn decode<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, McpError> {
    serde_json::from_value(value).map_err(|_| McpError::invalid(format!("Invalid entry in MCP {method} result")))
}

pub(crate) fn tools(items: Vec<Value>) -> Result<Vec<Tool>, McpError> {
    items.into_iter().map(|item| decode("tools/list", item)).collect()
}

/// `name` is required by the spec, but some servers omit it; the URI stands in.
fn with_name(mut item: Value, fallback_key: &str) -> Value {
    if let Some(object) = item.as_object_mut()
        && !object.contains_key("name")
        && let Some(fallback) = object.get(fallback_key).cloned()
    {
        object.insert("name".into(), fallback);
    }
    item
}

pub(crate) fn resources(items: Vec<Value>) -> Result<Vec<Resource>, McpError> {
    items.into_iter().map(|item| decode("resources/list", with_name(item, "uri"))).collect()
}

pub(crate) fn resource_templates(items: Vec<Value>) -> Result<Vec<ResourceTemplate>, McpError> {
    items.into_iter().map(|item| decode("resources/templates/list", with_name(item, "uriTemplate"))).collect()
}

pub(crate) fn read_resource_result(value: Value) -> Result<ReadResourceResult, McpError> {
    let Some(contents) = value.get("contents").and_then(Value::as_array) else {
        return Err(McpError::invalid("Invalid MCP resources/read result"));
    };
    let valid = contents.iter().all(|contents| {
        contents.get("uri").is_some_and(Value::is_string)
            && (contents.get("text").is_some_and(Value::is_string)
                || contents.get("blob").is_some_and(Value::is_string))
    });
    if !valid {
        return Err(McpError::invalid("Invalid contents in MCP resources/read result"));
    }
    serde_json::from_value(value).map_err(|_| McpError::invalid("Invalid contents in MCP resources/read result"))
}

/// `content` is required by the spec, but servers that only return `structuredContent` omit it.
pub(crate) fn call_tool_result(value: Value) -> Result<CallToolResult, McpError> {
    let invalid = || McpError::invalid("Invalid MCP tools/call result");
    let object = value.as_object().ok_or_else(invalid)?;
    if object.get("content").is_some_and(|content| !content.is_array()) {
        return Err(invalid());
    }
    if object.get("structuredContent").is_some_and(|structured| !structured.is_object()) {
        return Err(McpError::invalid("Invalid MCP tools/call structured content"));
    }
    serde_json::from_value(value).map_err(|_| invalid())
}
