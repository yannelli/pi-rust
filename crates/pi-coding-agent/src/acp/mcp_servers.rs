//! MCP servers an ACP client supplies in `session/new` and `session/load`. pi supports stdio only
//! (`mcpCapabilities.http` and `sse` are false) and keeps each value exactly as sent, in memory only.

use std::fmt;
use std::path::Path;

use agent_client_protocol_schema::{Error, McpServer, McpServerStdio, Meta};
use pi_mcp::{McpConfigScope, McpExposure, McpServerConfig, McpServerEntry, server_key};
use serde_json::{Map, Value};

use super::errors::{AcpResult, invalid_params};

/// The source of client servers in `/mcp` and notices.
pub const CLIENT_SOURCE: &str = "ACP client";

/// The checked servers of one `session/new` or `session/load`. Its `Debug` shows names and counts only.
#[derive(Clone, Default, PartialEq)]
pub struct ClientMcpServers(Vec<McpServerEntry>);

impl ClientMcpServers {
    /// Check every server and the request's `_meta` before anything starts. Errors name the server,
    /// never a value it holds.
    pub fn from_request(servers: &[McpServer], meta: Option<&Meta>) -> AcpResult<Self> {
        let exposure = client_exposure(meta)?;
        let mut entries: Vec<McpServerEntry> = Vec::new();
        for (index, server) in servers.iter().enumerate() {
            let McpServer::Stdio(stdio) = server else {
                return Err(rejected(index, server_name(server), unsupported(server)));
            };
            let mut entry = stdio_entry(stdio).map_err(|problem| rejected(index, &stdio.name, &problem))?;
            entry.config.exposure = exposure;
            let key = server_key(&entry.name);
            if let Some(other) = entries.iter().find(|other| server_key(&other.name) == key) {
                let problem = format!("its tool names would be the same as those of \"{}\"", other.name);
                return Err(rejected(index, &stdio.name, &problem));
            }
            entries.push(entry);
        }
        Ok(Self(entries))
    }

    pub fn entries(&self) -> &[McpServerEntry] {
        &self.0
    }

    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|entry| entry.name.as_str()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ClientMcpServers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let servers = self.0.iter().map(|entry| {
            let args = entry.config.args.as_ref().map_or(0, Vec::len);
            let env = entry.config.env.as_ref().map_or(0, Map::len);
            format!("{} ({args} args, {env} env)", entry.name)
        });
        formatter.debug_list().entries(servers).finish()
    }
}

/// `_meta.piAcp.clientMcpExposure`: absent, or `"direct"` to offer the client's tools to the model
/// directly instead of through codemode. Other values are rejected.
fn client_exposure(meta: Option<&Meta>) -> AcpResult<Option<McpExposure>> {
    let Some(pi) = meta.and_then(|meta| meta.get("piAcp")) else { return Ok(None) };
    let Some(pi) = pi.as_object() else { return Err(invalid_params("_meta.piAcp must be an object")) };
    match pi.get("clientMcpExposure") {
        None => Ok(None),
        Some(Value::String(value)) if value == "direct" => Ok(Some(McpExposure::Direct)),
        Some(_) => Err(invalid_params("_meta.piAcp.clientMcpExposure must be \"direct\"")),
    }
}

fn rejected(index: usize, name: &str, problem: &str) -> Error {
    invalid_params(format!("mcpServers[{index}] (\"{name}\"): {problem}"))
}

fn server_name(server: &McpServer) -> &str {
    match server {
        McpServer::Http(http) => &http.name,
        McpServer::Sse(sse) => &sse.name,
        McpServer::Stdio(stdio) => &stdio.name,
        _ => "",
    }
}

fn unsupported(server: &McpServer) -> &'static str {
    match server {
        McpServer::Http(_) => "HTTP transport is not supported (mcpCapabilities.http is false)",
        McpServer::Sse(_) => "SSE transport is not supported (mcpCapabilities.sse is false)",
        _ => "this transport is not supported; use stdio",
    }
}

/// A `Client` entry with the command, args, and env exactly as sent.
fn stdio_entry(server: &McpServerStdio) -> Result<McpServerEntry, String> {
    if server.name.trim().is_empty() {
        return Err("the name is empty".into());
    }
    let Some(command) = server.command.to_str() else {
        return Err("the command is not valid UTF-8".into());
    };
    if command.contains('\0') {
        return Err("the command contains a NUL byte".into());
    }
    if !Path::new(command).is_absolute() {
        return Err("the command must be an absolute path".into());
    }
    if server.args.iter().any(|arg| arg.contains('\0')) {
        return Err("an argument contains a NUL byte".into());
    }
    let mut env = Map::new();
    for variable in &server.env {
        let name = &variable.name;
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(format!("the env name {name:?} is not valid"));
        }
        if variable.value.contains('\0') {
            return Err(format!("env \"{name}\" contains a NUL byte"));
        }
        if env.insert(name.clone(), Value::String(variable.value.clone())).is_some() {
            return Err(format!("env \"{name}\" is set more than once"));
        }
    }
    let mut config = McpServerConfig::stdio(command, server.args.clone());
    config.env = (!env.is_empty()).then_some(env);
    let mut entry = McpServerEntry::new(server.name.clone(), config, CLIENT_SOURCE);
    entry.scope = Some(McpConfigScope::Client);
    Ok(entry)
}
