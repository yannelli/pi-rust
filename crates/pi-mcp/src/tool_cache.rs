//! Tool lists of configured servers, saved in `<agent dir>/mcp-cache/<server>.json` whenever a
//! server lists its tools, so `lazy` servers register their tools before they connect. The file
//! holds a fingerprint of the connection settings; a config change makes the list invalid.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{McpConfigScope, McpServerEntry};
use crate::types::Tool;

const CACHE_DIR: &str = "mcp-cache";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedServerTools {
    pub fingerprint: String,
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Servers from config files. Extension and ACP client servers are not cached.
fn is_cached(entry: &McpServerEntry) -> bool {
    entry.scope.is_none_or(McpConfigScope::is_saved)
}

/// SHA-256 of the config file and the settings that decide which server pi connects to.
pub fn connection_fingerprint(entry: &McpServerEntry) -> String {
    let config = &entry.config;
    let settings = json!([
        entry.source,
        config.kind,
        config.command,
        config.args,
        config.env,
        config.cwd,
        config.url,
        config.headers,
        config.oauth,
        config.auth,
    ]);
    let digest = ring::digest::digest(&ring::digest::SHA256, settings.to_string().as_bytes());
    digest.as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cache_path(agent_dir: &Path, server: &str) -> PathBuf {
    agent_dir.join(CACHE_DIR).join(format!("{server}.json"))
}

/// The saved tool list of the server, when its fingerprint matches the current config.
pub fn read_cached_tools(agent_dir: &Path, entry: &McpServerEntry) -> Option<CachedServerTools> {
    if !is_cached(entry) {
        return None;
    }
    let text = std::fs::read_to_string(cache_path(agent_dir, &entry.name)).ok()?;
    let cached: CachedServerTools = serde_json::from_str(&text).ok()?;
    (cached.fingerprint == connection_fingerprint(entry)).then_some(cached)
}

/// Save the tool list of the server. The file is replaced in one rename, so other pi processes
/// read the old or the new list.
pub fn write_cached_tools(
    agent_dir: &Path,
    entry: &McpServerEntry,
    tools: &[Tool],
    instructions: Option<&str>,
) -> std::io::Result<()> {
    if !is_cached(entry) {
        return Ok(());
    }
    let cached = CachedServerTools {
        fingerprint: connection_fingerprint(entry),
        tools: tools.to_vec(),
        instructions: instructions.map(str::to_string),
    };
    let path = cache_path(agent_dir, &entry.name);
    let dir = agent_dir.join(CACHE_DIR);
    std::fs::create_dir_all(&dir)?;
    let temp = dir.join(format!(".{}.{}.tmp", entry.name, std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(&cached)?)?;
    std::fs::rename(&temp, &path)
}
