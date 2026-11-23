//! Tool lists of configured servers, saved in `<agent dir>/mcp-cache/<server>.json` whenever a
//! server lists its tools, so `lazy` servers register their tools before they connect. The file
//! holds a fingerprint of the connection settings; a config change makes the list invalid.

use std::fmt::Write;
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

/// Longest file stem, so the stem with `.json` or the temporary file suffix fits in the 255 bytes
/// file systems allow for a name.
const MAX_FILE_STEM_BYTES: usize = 200;

/// The server name with each byte outside `[A-Za-z0-9_-]` as `%XX`, so every name is one file name
/// in the cache directory and no two names share a file. A longer stem is cut and ends in `%` and
/// a hash of the name.
fn file_stem(server: &str) -> String {
    let mut stem = String::new();
    for byte in server.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            stem.push(char::from(byte));
        } else {
            let _ = write!(stem, "%{byte:02X}");
        }
    }
    if stem.len() > MAX_FILE_STEM_BYTES {
        let digest = ring::digest::digest(&ring::digest::SHA256, server.as_bytes());
        let hash: String = digest.as_ref()[..8].iter().map(|byte| format!("{byte:02x}")).collect();
        stem.truncate(MAX_FILE_STEM_BYTES - hash.len() - 1);
        stem.push('%');
        stem.push_str(&hash);
    }
    stem
}

fn cache_path(agent_dir: &Path, server: &str) -> PathBuf {
    agent_dir.join(CACHE_DIR).join(format!("{}.json", file_stem(server)))
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
    let temp = dir.join(format!(".{}.{}.tmp", file_stem(&entry.name), std::process::id()));
    std::fs::write(&temp, serde_json::to_vec_pretty(&cached)?)?;
    std::fs::rename(&temp, &path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::McpServerConfig;

    #[test]
    fn keeps_every_server_name_in_its_own_file_in_the_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tools = [Tool { name: "search".into(), ..Tool::default() }];
        for (name, file) in [
            ("my-server_1", "my-server_1.json"),
            ("plugin_privacy-legal_Google Drive", "plugin_privacy-legal_Google%20Drive.json"),
            ("../x", "%2E%2E%2Fx.json"),
            ("é", "%C3%A9.json"),
        ] {
            let entry = McpServerEntry::new(name, McpServerConfig::default(), "mcp.json");
            write_cached_tools(dir.path(), &entry, &tools, None).unwrap();
            assert!(dir.path().join(CACHE_DIR).join(file).is_file(), "{name}");
            assert_eq!(read_cached_tools(dir.path(), &entry).unwrap().tools, tools);
        }
        assert!(!dir.path().join("x.json").exists());

        // 21 four-byte characters would need 252 bytes as `%XX`.
        for name in ["😀".repeat(21), "😀".repeat(22)] {
            let entry = McpServerEntry::new(name.as_str(), McpServerConfig::default(), "mcp.json");
            assert_eq!(file_stem(&name).len(), MAX_FILE_STEM_BYTES);
            write_cached_tools(dir.path(), &entry, &tools, None).unwrap();
            assert_eq!(read_cached_tools(dir.path(), &entry).unwrap().tools, tools);
        }
        assert_ne!(file_stem(&"😀".repeat(21)), file_stem(&"😀".repeat(22)));
    }
}
