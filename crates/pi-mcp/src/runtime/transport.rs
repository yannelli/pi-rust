//! Transports built from server configs: stdio for `command`, streamable HTTP for `url`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use url::Url;

use crate::auth::AuthProvider;
use crate::config::McpServerEntry;
use crate::config_value::{resolve_config_value_or_err, resolve_headers_or_err};
use crate::error::McpError;
use crate::transport::SharedTransport;
use crate::transport::http::{StreamableHttpOptions, StreamableHttpTransport};
use crate::transport::stdio::{StderrMode, StdioTransport, StdioTransportOptions};

/// Builds the transport of a server. The app can replace it, for example for tests.
pub type TransportFactory = Arc<
    dyn Fn(&McpServerEntry, &Path, Option<Arc<dyn AuthProvider>>) -> Result<SharedTransport, McpError> + Send + Sync,
>;

pub fn home_dir() -> Option<PathBuf> {
    let variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(variable).filter(|home| !home.is_empty()).map(PathBuf::from)
}

/// `~` and `~/...` (also `~\...` on Windows) name `home`, like in a shell.
pub fn expand_home_with(value: &str, home: Option<&Path>) -> String {
    let Some(home) = home else { return value.to_string() };
    if value == "~" {
        return home.display().to_string();
    }
    let rest = value.strip_prefix("~/").or_else(|| if cfg!(windows) { value.strip_prefix("~\\") } else { None });
    match rest {
        Some(rest) => home.join(rest).display().to_string(),
        None => value.to_string(),
    }
}

pub fn expand_home(value: &str) -> String {
    expand_home_with(value, home_dir().as_deref())
}

/// Resolve `${VAR}` and `!command` in env values and headers, and expand `~`.
pub fn create_default_transport(
    entry: &McpServerEntry,
    cwd: &Path,
    auth_provider: Option<Arc<dyn AuthProvider>>,
) -> Result<SharedTransport, McpError> {
    let config = &entry.config;
    let name = &entry.name;
    if let Some(url) = &config.url {
        let headers = resolve_headers_or_err(config.header_pairs(), &format!("MCP server \"{name}\""))
            .map_err(McpError::Other)?;
        let url = Url::parse(url).map_err(|error| McpError::other(format!("Invalid URL {url}: {error}")))?;
        let mut options = StreamableHttpOptions::new(url);
        options.headers = headers;
        options.auth_provider = auth_provider;
        return Ok(Arc::new(StreamableHttpTransport::new(options)?));
    }
    let mut env = std::collections::BTreeMap::new();
    for (key, value) in config.env_pairs() {
        let description = format!("MCP server \"{name}\" env \"{key}\"");
        env.insert(key.to_string(), resolve_config_value_or_err(value, &description, None).map_err(McpError::Other)?);
    }
    let command = expand_home(config.command.as_deref().unwrap_or_default());
    let args = config.args.iter().flatten().map(|arg| expand_home(arg)).collect();
    let mut options = StdioTransportOptions::new(command, args);
    options.cwd = Some(cwd.join(expand_home(config.cwd.as_deref().unwrap_or("."))));
    options.env = env;
    options.stderr = StderrMode::Pipe;
    Ok(Arc::new(StdioTransport::new(options)))
}

pub fn default_transport_factory() -> TransportFactory {
    Arc::new(create_default_transport)
}

/// The `session/new` entry for an agent that connects to this server itself (Antigravity ACP), with
/// the same value resolution. `None` for disabled or hidden servers and for HTTP servers whose
/// login pi holds (OAuth or a pi provider credential).
pub fn acp_passthrough_server(
    entry: &McpServerEntry,
) -> Result<Option<pi_ai::providers::antigravity_acp::PassthroughMcpServer>, McpError> {
    let config = &entry.config;
    let pi_login = config.is_http() && (config.uses_oauth() || config.auth_provider().is_some());
    if !config.is_enabled() || config.exposure() == crate::config::McpExposure::Hidden || pi_login {
        return Ok(None);
    }
    let pairs = |values: Vec<(String, String)>| -> Vec<serde_json::Value> {
        values.into_iter().map(|(name, value)| serde_json::json!({"name": name, "value": value})).collect()
    };
    let name = &entry.name;
    let acp = match &config.url {
        Some(url) => {
            let headers = resolve_headers_or_err(config.header_pairs(), &format!("MCP server \"{name}\""))
                .map_err(McpError::Other)?;
            let kind = if config.kind.as_deref() == Some("sse") { "sse" } else { "http" };
            serde_json::json!({"type": kind, "name": name, "url": url, "headers": pairs(headers.into_iter().collect())})
        }
        None => {
            let mut env = Vec::new();
            for (key, value) in config.env_pairs() {
                let description = format!("MCP server \"{name}\" env \"{key}\"");
                env.push((
                    key.to_string(),
                    resolve_config_value_or_err(value, &description, None).map_err(McpError::Other)?,
                ));
            }
            let args: Vec<String> = config.args.iter().flatten().map(|arg| expand_home(arg)).collect();
            let command = expand_home(config.command.as_deref().unwrap_or_default());
            serde_json::json!({"name": name, "command": command, "args": args, "env": pairs(env)})
        }
    };
    let tool_prefix = crate::naming::create_mcp_tool_name(name, "", |_| false);
    Ok(Some(pi_ai::providers::antigravity_acp::PassthroughMcpServer { acp, tool_prefix }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home_like_a_shell() {
        let home = Path::new("/home/me");
        assert_eq!(expand_home_with("~", Some(home)), "/home/me");
        assert_eq!(
            expand_home_with("~/server.js", Some(home)),
            Path::new("/home/me").join("server.js").display().to_string()
        );
        assert_eq!(expand_home_with("~other/x", Some(home)), "~other/x");
        assert_eq!(expand_home_with("a/~/b", Some(home)), "a/~/b");
    }

    fn entry(name: &str, config: crate::config::McpServerConfig) -> McpServerEntry {
        McpServerEntry { name: name.into(), config, source: "test".into(), scope: None, override_path: None }
    }

    #[test]
    fn acp_passthrough_keeps_servers_the_agent_can_reach_itself() {
        use crate::config::McpServerConfig;
        let mut stdio = McpServerConfig::stdio("docs-mcp", vec!["--port".into()]);
        stdio.env = Some(serde_json::json!({"MODE": "fast"}).as_object().cloned().unwrap());
        let server = acp_passthrough_server(&entry("my docs", stdio.clone())).unwrap().unwrap();
        assert_eq!(server.tool_prefix, "mcp__my_docs__");
        let expected = serde_json::json!({"name": "my docs", "command": "docs-mcp", "args": ["--port"],
            "env": [{"name": "MODE", "value": "fast"}]});
        assert_eq!(server.acp, expected);
        let disabled = McpServerConfig { enabled: Some(false), ..stdio };
        assert!(acp_passthrough_server(&entry("off", disabled)).unwrap().is_none());
        let oauth = McpServerConfig::http("https://mcp.example.com");
        assert!(acp_passthrough_server(&entry("oauth", oauth)).unwrap().is_none());
        let mut keyed = McpServerConfig::http("https://mcp.example.com");
        keyed.headers = Some(serde_json::json!({"Authorization": "Bearer k"}).as_object().cloned().unwrap());
        let keyed = acp_passthrough_server(&entry("keyed", keyed)).unwrap().unwrap();
        assert_eq!(keyed.acp["type"], "http");
        assert_eq!(keyed.acp["headers"], serde_json::json!([{"name": "Authorization", "value": "Bearer k"}]));
    }
}
