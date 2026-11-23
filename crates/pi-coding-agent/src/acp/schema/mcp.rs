use std::path::PathBuf;

use serde::Deserialize;

use super::Meta;

/// An MCP server the client supplies; without a known `type` it is a stdio server.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpServer {
    Http(McpServerHttp),
    Sse(McpServerSse),
    #[serde(untagged)]
    Stdio(McpServerStdio),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct McpServerHttp {
    pub name: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct McpServerSse {
    pub name: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct McpServerStdio {
    pub name: String,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<EnvVariable>,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct EnvVariable {
    pub name: String,
    pub value: String,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
    #[serde(rename = "_meta")]
    pub meta: Option<Meta>,
}
