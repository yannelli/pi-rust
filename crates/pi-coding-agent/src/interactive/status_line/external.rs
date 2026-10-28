//! Status lines from other agents' settings: the `statusLine` command of Claude Code
//! (`~/.claude/settings.json`) or the Cursor CLI (`~/.cursor/cli-config.json`), and the
//! `[tui] status_line` items of Codex (`~/.codex/config.toml`).

use std::path::{Path, PathBuf};

use serde_json::Value;

/// The `statusLine` command, padding, and refresh interval of a Claude Code or Cursor CLI file.
#[derive(Clone, Debug, PartialEq)]
pub struct ExternalCommand {
    pub command: String,
    pub padding: Option<f64>,
    pub refresh_interval: Option<f64>,
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
pub fn claude_settings_path(home: &Path) -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude")).join("settings.json")
}

/// `~/.cursor/cli-config.json`.
pub fn cursor_config_path(home: &Path) -> PathBuf {
    home.join(".cursor").join("cli-config.json")
}

/// `$CODEX_HOME/config.toml`, else `~/.codex/config.toml`.
pub fn codex_config_path(home: &Path) -> PathBuf {
    env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex")).join("config.toml")
}

/// The `statusLine` command in a JSON settings file, when it is a non-empty command.
pub fn read_command(path: &Path) -> Option<ExternalCommand> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let status_line = value.get("statusLine")?;
    let command = status_line.get("command")?.as_str()?.trim();
    if command.is_empty() || status_line.get("type").and_then(Value::as_str).is_some_and(|kind| kind != "command") {
        return None;
    }
    Some(ExternalCommand {
        command: command.to_string(),
        padding: status_line.get("padding").and_then(Value::as_f64),
        refresh_interval: status_line.get("refreshInterval").and_then(Value::as_f64),
    })
}

/// The `status_line` item names of a Codex `config.toml`, read from `[tui]` or `tui.status_line`.
pub fn read_codex_items(path: &Path) -> Option<Vec<String>> {
    parse_codex_items(&std::fs::read_to_string(path).ok()?)
}

/// A small reader for the one array that matters; it does not parse other TOML.
pub fn parse_codex_items(toml: &str) -> Option<Vec<String>> {
    let mut section = String::new();
    let mut lines = toml.lines();
    while let Some(line) = lines.next() {
        let line = strip_comment(line).trim();
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            section = name.trim().to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        let matches = (section == "tui" && key == "status_line") || (section.is_empty() && key == "tui.status_line");
        if !matches {
            continue;
        }
        let mut array = value.trim().to_string();
        while !array.contains(']') {
            array.push_str(strip_comment(lines.next()?));
        }
        return Some(quoted_strings(&array));
    }
    None
}

/// The line without a `#` comment outside quotes.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (index, ch) in line.char_indices() {
        match (quote, ch) {
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), _) if ch == open => quote = None,
            (None, '#') => return &line[..index],
            _ => {}
        }
    }
    line
}

fn quoted_strings(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '"' || ch == '\'' {
            let item: String = chars.by_ref().take_while(|next| *next != ch).collect();
            items.push(item);
        }
    }
    items
}
