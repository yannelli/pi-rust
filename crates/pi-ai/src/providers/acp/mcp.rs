//! The user's MCP servers in `session/new` and `session/load`: the servers pi passes through
//! (enabled, trusted, and without a pi-held login), filtered by the `mcpCapabilities` of the agent.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::providers::antigravity_acp::passthrough;

/// The servers for the agent and a notice for each server it cannot take.
pub(crate) fn servers(capabilities: &Value, agent_name: &str) -> (Vec<Value>, Vec<String>) {
    select(passthrough().into_iter().map(|server| server.acp).collect(), capabilities, agent_name)
}

pub(crate) fn select(servers: Vec<Value>, capabilities: &Value, agent_name: &str) -> (Vec<Value>, Vec<String>) {
    let path = std::env::var_os("PATH");
    let (mut kept, mut notices) = (Vec::new(), Vec::new());
    for mut server in servers {
        let name = server["name"].as_str().unwrap_or_default().to_string();
        match server["type"].as_str() {
            Some(kind @ ("http" | "sse")) => {
                if capabilities.pointer(&format!("/mcpCapabilities/{kind}")) != Some(&Value::Bool(true)) {
                    notices
                        .push(format!("{agent_name} does not take {kind} MCP servers, so it does not get \"{name}\"."));
                    continue;
                }
            }
            _ => {
                let command = server["command"].as_str().unwrap_or_default();
                if let Some(resolved) = absolute(command, path.as_deref()) {
                    server["command"] = Value::String(resolved.display().to_string());
                }
            }
        }
        kept.push(server);
    }
    (kept, notices)
}

/// ACP wants an absolute command path; a bare name is looked up on `path`.
fn absolute(command: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if command.is_empty() || Path::new(command).is_absolute() || command.contains(['/', '\\']) {
        return None;
    }
    let names: Vec<String> = match cfg!(windows) {
        true => vec![format!("{command}.exe"), format!("{command}.cmd"), command.to_string()],
        false => vec![command.to_string()],
    };
    let dirs = std::env::split_paths(path?).collect::<Vec<_>>();
    dirs.iter().flat_map(|dir| names.iter().map(move |name| dir.join(name))).find(|file| file.is_file())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn remote_servers_need_the_agent_capability() {
        let servers = vec![
            json!({"name": "local", "command": "/bin/echo", "args": [], "env": []}),
            json!({"type": "http", "name": "web", "url": "http://127.0.0.1:9/mcp", "headers": []}),
            json!({"type": "sse", "name": "events", "url": "http://127.0.0.1:9/sse", "headers": []}),
        ];
        let (kept, notices) = select(servers.clone(), &json!({"mcpCapabilities": {"http": true}}), "Kiro");
        let names: Vec<&str> = kept.iter().filter_map(|server| server["name"].as_str()).collect();
        assert_eq!(names, ["local", "web"]);
        assert_eq!(notices, ["Kiro does not take sse MCP servers, so it does not get \"events\"."]);
        assert_eq!(select(servers, &json!({}), "Kiro").0.len(), 1);
    }

    #[test]
    fn bare_commands_resolve_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(dir.path().join(name), "").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(absolute("tool", Some(&path)), Some(dir.path().join(name)));
        assert_eq!(absolute("missing", Some(&path)), None);
        assert_eq!(absolute("./tool", Some(&path)), None);
    }
}
