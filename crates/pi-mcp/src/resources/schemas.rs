//! Descriptions and schemas of the resource tools, as Codex declares them.

use serde_json::{Value, json};

pub const LIST_DESCRIPTION: &str = "Lists resources provided by MCP servers. Resources allow servers to share data that provides context to language models, such as files, database schemas, or application-specific information. Prefer resources over web search when possible.";
pub const LIST_TEMPLATES_DESCRIPTION: &str = "Lists resource templates provided by MCP servers. Parameterized resource templates allow servers to share data that takes parameters and provides context to language models, such as files, database schemas, or application-specific information. Prefer resource templates over web search when possible.";
pub const READ_DESCRIPTION: &str =
    "Read a specific resource from an MCP server given the server name and resource URI.";

fn string(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

pub fn list_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string("MCP server name. Omit to list every server with resources."),
            "cursor": string("Opaque cursor from a previous call with the same server; omit for the first page.")
        },
        "additionalProperties": false
    })
}

pub fn read_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": string("MCP server name exactly as configured. Must match the 'server' field returned by list_mcp_resources."),
            "uri": string("Resource URI to read. Must be one of the URIs returned by list_mcp_resources.")
        },
        "required": ["server", "uri"],
        "additionalProperties": false
    })
}

fn listing_errors() -> Value {
    json!({
        "type": "array",
        "description": "Servers that could not be listed",
        "items": {
            "type": "object",
            "properties": { "server": { "type": "string" }, "error": { "type": "string" } },
            "required": ["server", "error"]
        }
    })
}

pub fn list_output() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "resources": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uri": { "type": "string" },
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "mimeType": { "type": "string" },
                        "size": { "type": "number" }
                    },
                    "required": ["server", "uri", "name"]
                }
            },
            "nextCursor": { "type": "string" },
            "errors": listing_errors()
        },
        "required": ["resources"]
    })
}

pub fn list_templates_output() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "resourceTemplates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "server": { "type": "string" },
                        "uriTemplate": { "type": "string", "description": "RFC 6570 URI template" },
                        "name": { "type": "string" },
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "mimeType": { "type": "string" }
                    },
                    "required": ["server", "uriTemplate", "name"]
                }
            },
            "nextCursor": { "type": "string" },
            "errors": listing_errors()
        },
        "required": ["resourceTemplates"]
    })
}

pub fn read_output() -> Value {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string" },
            "uri": { "type": "string" },
            "contents": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "uri": { "type": "string" }, "mimeType": { "type": "string" }, "text": { "type": "string" } },
                            "required": ["uri", "text"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "uri": { "type": "string" },
                                "mimeType": { "type": "string" },
                                "blob": { "type": "string", "description": "base64" }
                            },
                            "required": ["uri", "blob"]
                        }
                    ]
                }
            }
        },
        "required": ["server", "uri", "contents"]
    })
}
