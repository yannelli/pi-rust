//! One-time migrations that run on startup (upstream `migrations.ts`): legacy credentials move
//! to `auth.json`, stray session files move under `sessions/`, `commands/` becomes `prompts/`,
//! managed binaries move from `tools/` to `bin/`, and `keybindings.json` gets current names.

use std::path::Path;

use serde_json::{Map, Value};

use crate::config::{auth_path, settings_path};
use crate::keybindings::migrate_keybindings_config_file;
use crate::resources::brand::project_config_dir;

const MIGRATION_GUIDE_URL: &str =
    "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/CHANGELOG.md#extensions-migration";
const EXTENSIONS_DOC_URL: &str =
    "https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md";
const MANAGED_BINARIES: [&str; 4] = ["fd", "rg", "fd.exe", "rg.exe"];

/// What [`run_migrations`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationResult {
    /// Providers whose credentials moved to `auth.json`.
    pub migrated_auth_providers: Vec<String>,
    /// Deprecated `hooks/` and custom `tools/` directories found.
    pub deprecation_warnings: Vec<String>,
}

/// Run every migration once on startup; `cwd` gives the project configuration directory.
pub fn run_migrations(cwd: &Path, agent_dir: &Path) -> MigrationResult {
    let migrated_auth_providers = migrate_auth_to_auth_json(agent_dir);
    migrate_sessions_from_agent_root(agent_dir);
    migrate_tools_to_bin(agent_dir);
    migrate_keybindings_config_file(agent_dir);
    let deprecation_warnings = migrate_extension_system(cwd, agent_dir);
    MigrationResult { migrated_auth_providers, deprecation_warnings }
}

fn read_json_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(text.strip_prefix('\u{feff}').unwrap_or(&text)) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Move `oauth.json` and the `apiKeys` of `settings.json` into a new `auth.json`. Returns the
/// migrated provider names; nothing happens once `auth.json` exists.
pub fn migrate_auth_to_auth_json(agent_dir: &Path) -> Vec<String> {
    let auth = auth_path(agent_dir);
    if auth.exists() {
        return Vec::new();
    }
    let mut migrated = Map::new();
    let mut providers = Vec::new();
    let oauth_path = agent_dir.join("oauth.json");
    if oauth_path.exists()
        && let Some(oauth) = read_json_object(&oauth_path)
    {
        for (provider, credential) in oauth {
            let mut entry = Map::from_iter([("type".to_string(), Value::from("oauth"))]);
            if let Value::Object(fields) = credential {
                entry.extend(fields);
            }
            migrated.insert(provider.clone(), Value::Object(entry));
            providers.push(provider);
        }
        let _ = std::fs::rename(&oauth_path, agent_dir.join("oauth.json.migrated"));
    }
    let settings = settings_path(agent_dir);
    if let Some(mut object) = read_json_object(&settings)
        && let Some(Value::Object(keys)) = object.get("apiKeys").cloned()
    {
        for (provider, key) in keys {
            if let Value::String(key) = key
                && !migrated.contains_key(&provider)
            {
                migrated.insert(provider.clone(), serde_json::json!({ "type": "api_key", "key": key }));
                providers.push(provider);
            }
        }
        object.remove("apiKeys");
        if let Ok(text) = serde_json::to_string_pretty(&Value::Object(object)) {
            let _ = std::fs::write(&settings, text);
        }
    }
    if !migrated.is_empty()
        && let Ok(text) = serde_json::to_string_pretty(&Value::Object(migrated))
    {
        let _ = std::fs::create_dir_all(agent_dir);
        if std::fs::write(&auth, text).is_ok() {
            restrict_permissions(&auth);
        }
    }
    providers
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Move `*.jsonl` session files saved in the agent directory itself (a v0.30.0 bug) into the
/// session directory of the cwd named by their header.
pub fn migrate_sessions_from_agent_root(agent_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(agent_dir) else { return };
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if path.extension().is_none_or(|extension| extension != "jsonl") || !path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let first = content.split('\n').next().unwrap_or_default();
        let Ok(header) = serde_json::from_str::<Value>(first) else { continue };
        let cwd = header.get("cwd").and_then(Value::as_str).filter(|cwd| !cwd.is_empty());
        let (Some("session"), Some(cwd)) = (header.get("type").and_then(Value::as_str), cwd) else { continue };
        let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
        let dir = agent_dir.join("sessions").join(format!("--{}--", trimmed.replace(['/', '\\', ':'], "-")));
        let Some(name) = path.file_name() else { continue };
        let target = dir.join(name);
        if std::fs::create_dir_all(&dir).is_ok() && !target.exists() {
            let _ = std::fs::rename(&path, &target);
        }
    }
}

/// Rename `<base>/commands` to `<base>/prompts` unless `prompts` exists.
fn migrate_commands_to_prompts(base: &Path, label: &str) -> bool {
    let (commands, prompts) = (base.join("commands"), base.join("prompts"));
    if !commands.exists() || prompts.exists() {
        return false;
    }
    match std::fs::rename(&commands, &prompts) {
        Ok(()) => {
            crate::diagnostics::say_stdout(&format!("\x1b[32mMigrated {label} commands/ → prompts/\x1b[39m"));
            true
        }
        Err(error) => {
            crate::diagnostics::say_stdout(&format!(
                "\x1b[33mWarning: Could not migrate {label} commands/ to prompts/: {error}\x1b[39m"
            ));
            false
        }
    }
}

/// Move the fd and rg binaries from `tools/` to `bin/`, deleting old copies `bin/` already has.
fn migrate_tools_to_bin(agent_dir: &Path) {
    let (tools, bin) = (agent_dir.join("tools"), agent_dir.join("bin"));
    if !tools.exists() {
        return;
    }
    let mut moved_any = false;
    for name in MANAGED_BINARIES {
        let (old, new) = (tools.join(name), bin.join(name));
        if !old.exists() {
            continue;
        }
        let _ = std::fs::create_dir_all(&bin);
        if new.exists() {
            let _ = std::fs::remove_file(&old);
        } else if std::fs::rename(&old, &new).is_ok() {
            moved_any = true;
        }
    }
    if moved_any {
        crate::diagnostics::say_stdout("\x1b[32mMigrated managed binaries tools/ → bin/\x1b[39m");
    }
}

/// Warnings for a `hooks/` directory, and for a `tools/` directory with more than fd and rg.
fn check_deprecated_extension_dirs(base: &Path, label: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if base.join("hooks").exists() {
        warnings.push(format!("{label} hooks/ directory found. Hooks have been renamed to extensions."));
    }
    if let Ok(entries) = std::fs::read_dir(base.join("tools")) {
        let custom = entries.filter_map(Result::ok).any(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            !MANAGED_BINARIES.contains(&name.to_lowercase().as_str()) && !name.starts_with('.')
        });
        if custom {
            warnings.push(format!(
                "{label} tools/ directory contains custom tools. Custom tools have been merged into extensions."
            ));
        }
    }
    warnings
}

fn migrate_extension_system(cwd: &Path, agent_dir: &Path) -> Vec<String> {
    migrate_commands_to_prompts(agent_dir, "Global");
    migrate_commands_to_prompts(&project_config_dir(cwd), "Project");
    deprecation_warnings(cwd, agent_dir)
}

/// The deprecated `hooks/` and custom `tools/` directories [`run_migrations`] warns about, found
/// without migrating anything.
pub fn deprecation_warnings(cwd: &Path, agent_dir: &Path) -> Vec<String> {
    let mut warnings = check_deprecated_extension_dirs(agent_dir, "Global");
    warnings.extend(check_deprecated_extension_dirs(&project_config_dir(cwd), "Project"));
    warnings
}

/// The deprecation notice printed before the "Press any key" wait (upstream `showDeprecationWarnings`).
pub fn deprecation_warning_text(warnings: &[String]) -> String {
    let (yellow, dim, reset) = ("\x1b[33m", "\x1b[2m", "\x1b[0m");
    let mut text = String::new();
    for warning in warnings {
        text.push_str(&format!("{yellow}Warning: {warning}{reset}\n"));
    }
    text.push_str(&format!("{yellow}\nMove your extensions to the extensions/ directory.{reset}\n"));
    text.push_str(&format!("{yellow}Migration guide: {MIGRATION_GUIDE_URL}{reset}\n"));
    text.push_str(&format!("{yellow}Documentation: {EXTENSIONS_DOC_URL}{reset}\n"));
    text.push_str(&format!("{dim}\nPress any key to continue...{reset}\n"));
    text
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
