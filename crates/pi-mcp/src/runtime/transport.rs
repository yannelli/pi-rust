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
}
