//! MCP server configuration: the `mcpServers` entries of `mcp.json`.

mod files;
mod validate;

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use files::{
    CONFIG_DIR_NAME, LoadedMcpConfig, McpConfigScope, McpServerConfigPatch, McpServerEntry, add_mcp_server_config,
    load_mcp_config, remove_mcp_server_config, update_mcp_server_config,
};
pub use validate::{is_loopback_redirect_uri, resolve_exposure_aliases, validate_mcp_server_config};

/// How a server's tools reach the model: `codemode` scripts, `deferred` until `tool_search` loads
/// them, `direct` declarations like built-in tools, or `hidden` (unreachable).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpExposure {
    #[default]
    Codemode,
    Deferred,
    Direct,
    Hidden,
}

impl McpExposure {
    pub const ALL: [McpExposure; 4] = [Self::Codemode, Self::Deferred, Self::Direct, Self::Hidden];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codemode => "codemode",
            Self::Deferred => "deferred",
            Self::Direct => "direct",
            Self::Hidden => "hidden",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|exposure| exposure.as_str() == value)
    }
}

impl fmt::Display for McpExposure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// OAuth client settings for servers without dynamic client registration.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpOAuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// May reference environment variables (`${NAME}`) or commands (`!cmd`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
    /// Loopback redirect URI registered for `clientId`, like `http://localhost:8080/oauth/callback`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// `dcr` (default) or `cimd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_registration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_server_metadata_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderAuth {
    pub provider: String,
}

/// One server entry. `url` makes it a streamable HTTP server, else `command` runs a stdio server.
/// String maps keep their order, which matters for `toolExposure` patterns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProviderAuth>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure: Option<McpExposure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_exposure: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Per-request timeout in seconds. Progress notifications restart it. Default: 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<f64>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn string_pairs(map: &Option<Map<String, Value>>) -> Vec<(&str, &str)> {
    map.iter().flatten().filter_map(|(name, value)| Some((name.as_str(), value.as_str()?))).collect()
}

impl McpServerConfig {
    pub fn stdio(command: impl Into<String>, args: Vec<String>) -> Self {
        Self { command: Some(command.into()), args: (!args.is_empty()).then_some(args), ..Self::default() }
    }

    pub fn http(url: impl Into<String>) -> Self {
        Self { url: Some(url.into()), ..Self::default() }
    }

    pub fn is_http(&self) -> bool {
        self.url.is_some()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled != Some(false)
    }

    pub fn exposure(&self) -> McpExposure {
        self.exposure.unwrap_or_default()
    }

    pub fn env_pairs(&self) -> Vec<(&str, &str)> {
        string_pairs(&self.env)
    }

    pub fn header_pairs(&self) -> Vec<(&str, &str)> {
        string_pairs(&self.headers)
    }

    /// `toolExposure` entries in order.
    pub fn tool_exposures(&self) -> Vec<(&str, McpExposure)> {
        let entries = self.tool_exposure.iter().flatten();
        entries.filter_map(|(tool, value)| Some((tool.as_str(), McpExposure::parse(value.as_str()?)?))).collect()
    }

    /// The provider whose `/login` token the server receives, from `auth.provider`.
    pub fn auth_provider(&self) -> Option<&str> {
        self.auth.as_ref().map(|auth| auth.provider.as_str()).filter(|_| self.is_http())
    }

    /// HTTP servers use OAuth unless the config supplies an `Authorization` header or `auth`.
    pub fn uses_oauth(&self) -> bool {
        self.is_http()
            && self.auth.is_none()
            && !self.header_pairs().iter().any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    }

    /// The URL, or the command line, for lists.
    pub fn describe_transport(&self) -> String {
        match &self.url {
            Some(url) => url.clone(),
            None => {
                let mut parts = vec![self.command.clone().unwrap_or_default()];
                parts.extend(self.args.iter().flatten().cloned());
                parts.join(" ")
            }
        }
    }
}

/// Pattern where `*` matches any characters, as `toolExposure`, `--tools`, and `--exclude-tools` accept.
pub fn tool_pattern_matches(pattern: &str, name: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if name.len() < first.len() + last.len() || !name.starts_with(first) || !name.ends_with(last) {
        return false;
    }
    let mut rest = &name[first.len()..name.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    true
}

/// Whether a tool name matches any of the entries, each an exact name or a pattern.
pub fn tool_name_matches(entries: &[String], name: &str) -> bool {
    entries.iter().any(|entry| if entry.contains('*') { tool_pattern_matches(entry, name) } else { entry == name })
}

/// Exposure of one tool: its exact `toolExposure` entry, else the first matching pattern, else
/// the server's `exposure`.
pub fn get_mcp_tool_exposure(config: &McpServerConfig, tool: &str) -> McpExposure {
    let entries = config.tool_exposures();
    if let Some((_, exposure)) = entries.iter().find(|(name, _)| *name == tool) {
        return *exposure;
    }
    let pattern = entries.iter().find(|(pattern, _)| pattern.contains('*') && tool_pattern_matches(pattern, tool));
    pattern.map(|(_, exposure)| *exposure).unwrap_or_else(|| config.exposure())
}

/// Namespace of a server's tools: `mcp__<server>` with `-` replaced by `_`.
pub fn mcp_namespace(server: &str) -> String {
    format!("mcp__{}", server.replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_tool_patterns_with_only_star_special() {
        assert!(tool_pattern_matches("get_*", "get_issue"));
        assert!(tool_pattern_matches("*delete*", "delete_repo"));
        assert!(tool_pattern_matches("a*b*c", "abc"));
        assert!(!tool_pattern_matches("a*b*c", "acb"));
        assert!(!tool_pattern_matches("get_file.*", "get_file_x"));
        assert!(tool_name_matches(&["x".into(), "mcp__*".into()], "mcp__a__b"));
        assert_eq!(mcp_namespace("my-server"), "mcp__my_server");
    }
}
