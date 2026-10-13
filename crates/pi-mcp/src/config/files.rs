//! Reading and editing `mcp.json` files. Project entries replace global ones with the same name; a
//! project entry without `command`, `url`, or `type` overrides only `enabled`, `exposure`, and
//! `toolExposure` of the global server, keeping credentials the project could not set itself.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use super::{McpExposure, McpServerConfig, mcp_namespace, validate_mcp_server_config};

const OVERRIDE_KEYS: [&str; 3] = ["enabled", "exposure", "toolExposure"];
/// Name of the project config directory, `.pi`.
pub const CONFIG_DIR_NAME: &str = ".pi";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum McpConfigScope {
    Global,
    Project,
    /// Registered by an extension; changes are not saved.
    Extension,
}

impl fmt::Display for McpConfigScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::Extension => "extension",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpServerEntry {
    pub name: String,
    pub config: McpServerConfig,
    /// Config file that defined the entry, or the path of the extension that registered it.
    pub source: String,
    pub scope: Option<McpConfigScope>,
    /// Project `mcp.json` that overrides this global server's `enabled`, `exposure`, or `toolExposure`.
    pub override_path: Option<String>,
}

impl McpServerEntry {
    pub fn new(name: impl Into<String>, config: McpServerConfig, source: impl Into<String>) -> Self {
        Self { name: name.into(), config, source: source.into(), scope: None, override_path: None }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LoadedMcpConfig {
    /// Disabled servers are included, so they can be enabled again.
    pub servers: Vec<McpServerEntry>,
    /// Activate the codemode tool when `codemode` servers connect. Default: true.
    pub auto_enable_codemode: Option<bool>,
    pub errors: Vec<String>,
    /// The project `mcp.json` of a trusted project, where project overrides are saved.
    pub project_config: Option<PathBuf>,
}

fn is_override(value: &Map<String, Value>) -> bool {
    !value.contains_key("command") && !value.contains_key("url") && !value.contains_key("type")
}

fn read_config_file(path: &Path, scope: McpConfigScope, loaded: &mut LoadedMcpConfig) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let shown = path.display().to_string();
    let parsed: Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => return loaded.errors.push(format!("{shown}: {error}")),
    };
    let servers = parsed.get("mcpServers");
    if !parsed.is_object() || servers.is_some_and(|servers| !servers.is_object()) {
        return loaded.errors.push(format!("{shown}: expected an object with an \"mcpServers\" object"));
    }
    match parsed.get("autoEnableCodemode") {
        Some(Value::Bool(enabled)) => loaded.auto_enable_codemode = Some(*enabled),
        Some(_) => loaded.errors.push(format!("{shown}: autoEnableCodemode must be a boolean")),
        None => {}
    }
    for (name, value) in servers.and_then(Value::as_object).into_iter().flatten() {
        let position = loaded.servers.iter().position(|entry| entry.name == *name);
        if scope == McpConfigScope::Project
            && let Some(object) = value.as_object().filter(|object| is_override(object))
        {
            let extra = object.keys().any(|key| !OVERRIDE_KEYS.contains(&key.as_str()));
            match position {
                None => loaded.errors.push(format!(
                    "{shown}: server \"{name}\" needs \"command\" or \"url\", or a global server to override"
                )),
                Some(_) if extra => loaded
                    .errors
                    .push(format!("{shown}: server \"{name}\": an override can only set {}", OVERRIDE_KEYS.join(", "))),
                Some(position) => {
                    let base = &loaded.servers[position];
                    let mut merged = serde_json::to_value(&base.config).unwrap_or_default();
                    if let Some(merged) = merged.as_object_mut() {
                        merged.extend(object.iter().map(|(key, value)| (key.clone(), value.clone())));
                    }
                    match validate_mcp_server_config(name, &merged) {
                        Ok(config) => {
                            let entry = &mut loaded.servers[position];
                            entry.config = config;
                            entry.override_path = Some(shown.clone());
                        }
                        Err(error) => loaded.errors.push(format!("{shown}: {error}")),
                    }
                }
            }
            continue;
        }
        let config = match validate_mcp_server_config(name, value) {
            Ok(config) => config,
            Err(error) => {
                loaded.errors.push(format!("{shown}: {error}"));
                continue;
            }
        };
        // Names that differ only in `-` and `_` would share a namespace.
        let clash = loaded
            .servers
            .iter()
            .find(|entry| entry.name != *name && mcp_namespace(&entry.name) == mcp_namespace(name));
        if let Some(clash) = clash {
            loaded.errors.push(format!("{shown}: server \"{name}\" conflicts with \"{}\"", clash.name));
            continue;
        }
        if scope == McpConfigScope::Project && config.is_http() && config.auth.is_some() {
            loaded.errors.push(format!("{shown}: server \"{name}\": auth is only allowed in the global mcp.json"));
            continue;
        }
        let entry = McpServerEntry {
            name: name.clone(),
            config,
            source: shown.clone(),
            scope: Some(scope),
            override_path: None,
        };
        match position {
            Some(position) => loaded.servers[position] = entry,
            None => loaded.servers.push(entry),
        }
    }
}

/// Load `<agent_dir>/mcp.json` and, for trusted projects, `<cwd>/.pi/mcp.json`.
pub fn load_mcp_config(agent_dir: &Path, cwd: &Path, project_trusted: bool) -> LoadedMcpConfig {
    let mut loaded = LoadedMcpConfig::default();
    read_config_file(&agent_dir.join("mcp.json"), McpConfigScope::Global, &mut loaded);
    if project_trusted {
        let project = cwd.join(CONFIG_DIR_NAME).join("mcp.json");
        read_config_file(&project, McpConfigScope::Project, &mut loaded);
        loaded.project_config = Some(project);
    }
    loaded
}

/// Settings `/mcp` changes. `enabled: true` and `exposure: codemode` are the defaults and remove the key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpServerConfigPatch {
    pub enabled: Option<bool>,
    pub exposure: Option<McpExposure>,
}

/// Read an `mcp.json` (empty when missing), let `edit` change it, and write it back with its
/// indentation when `edit` returns true. Other content is kept.
fn edit_mcp_servers(
    path: &Path,
    edit: impl FnOnce(&mut Map<String, Value>) -> Result<bool, String>,
) -> Result<(), String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).ok();
    let mut parsed: Value = match &text {
        Some(text) => serde_json::from_str(text).map_err(|error| format!("{shown}: {error}"))?,
        None => Value::Object(Map::new()),
    };
    let invalid = || format!("{shown}: expected an object with an \"mcpServers\" object");
    let root = parsed.as_object_mut().ok_or_else(invalid)?;
    if root.get("mcpServers").is_some_and(|servers| !servers.is_object()) {
        return Err(invalid());
    }
    if !edit(root)? {
        return Ok(());
    }
    let indent = text
        .as_deref()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let trimmed = line.trim_start_matches([' ', '\t']);
                (trimmed.len() < line.len() && !trimmed.is_empty() && !trimmed.starts_with(char::is_whitespace))
                    .then(|| line[..line.len() - trimmed.len()].to_string())
            })
        })
        .unwrap_or_else(|| "  ".into());
    let mut output = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut serializer = serde_json::Serializer::with_formatter(&mut output, formatter);
    parsed.serialize(&mut serializer).map_err(|error| error.to_string())?;
    output.push(b'\n');
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, output).map_err(|error| error.to_string())
}

/// Change one server's settings in the file that defines or overrides it. With `as_override`, a
/// missing entry is added as an override. Overrides keep default values, since they replace the
/// global server's.
pub fn update_mcp_server_config(
    path: &Path,
    name: &str,
    patch: McpServerConfigPatch,
    as_override: bool,
) -> Result<(), String> {
    let shown = path.display().to_string();
    edit_mcp_servers(path, |root| {
        let servers = root.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
        let servers = servers.as_object_mut().ok_or_else(|| format!("{shown}: expected an object"))?;
        if !servers.contains_key(name) && as_override {
            servers.insert(name.into(), Value::Object(Map::new()));
        }
        let server = servers
            .get_mut(name)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| format!("{shown} does not define MCP server \"{name}\""))?;
        let keep_defaults = is_override(server);
        if let Some(enabled) = patch.enabled {
            if enabled && !keep_defaults {
                server.shift_remove("enabled");
            } else {
                server.insert("enabled".into(), Value::Bool(enabled));
            }
        }
        if let Some(exposure) = patch.exposure {
            if exposure == McpExposure::Codemode && !keep_defaults {
                server.shift_remove("exposure");
            } else {
                server.insert("exposure".into(), Value::from(exposure.as_str()));
            }
        }
        Ok(true)
    })
}

/// Add a server, creating the file when missing. Returns true when an entry was replaced.
pub fn add_mcp_server_config(path: &Path, name: &str, config: &Value) -> Result<bool, String> {
    let mut replaced = false;
    edit_mcp_servers(path, |root| {
        let servers = root.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
        let servers = servers.as_object_mut().ok_or_else(|| "expected an object".to_string())?;
        replaced = servers.insert(name.into(), config.clone()).is_some();
        Ok(true)
    })?;
    Ok(replaced)
}

/// Remove a server. Returns false when the file does not define it.
pub fn remove_mcp_server_config(path: &Path, name: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let mut removed = false;
    edit_mcp_servers(path, |root| {
        let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) else { return Ok(false) };
        removed = servers.shift_remove(name).is_some();
        Ok(removed)
    })?;
    Ok(removed)
}
