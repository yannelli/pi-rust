//! `/session`, `/changelog`, `/export`, and `/name` with the output of ACP mode (upstream
//! pi-acp), for RPC mode and ACP mode alike.

use std::path::Path;

use serde_json::Value;

use crate::acp::commands::parse_command_args;
use crate::agent_session::AgentSession;
use crate::platform::changelog::EMBEDDED_CHANGELOG;

const CHANGELOG_MAX_CHARS: usize = 20_000;

/// The `/session` summary of pi's session stats.
pub fn format_session_stats(stats: &Value) -> String {
    let mut lines = Vec::new();
    let text = |key: &str| stats[key].as_str().filter(|text| !text.is_empty());
    if let Some(id) = text("sessionId") {
        lines.push(format!("Session: {id}"));
    }
    if let Some(file) = text("sessionFile") {
        lines.push(format!("Session file: {file}"));
    }
    if let Some(count) = stats["totalMessages"].as_f64() {
        lines.push(format!("Messages: {count}"));
    }
    if let Some(cost) = stats["cost"].as_f64() {
        lines.push(format!("Cost: {cost}"));
    }
    let tokens = &stats["tokens"];
    if tokens.is_object() {
        let parts: Vec<String> = [("input", "in"), ("output", "out"), ("cacheRead", "cache read")]
            .into_iter()
            .chain([("cacheWrite", "cache write"), ("total", "total")])
            .filter_map(|(key, label)| tokens[key].as_f64().map(|value| format!("{label} {value}")))
            .collect();
        if !parts.is_empty() {
            lines.push(format!("Tokens: {}", parts.join(", ")));
        }
    }
    if lines.is_empty() {
        format!("Session stats:\n{}", serde_json::to_string_pretty(stats).unwrap_or_default())
    } else {
        lines.join("\n")
    }
}

/// The embedded changelog, cut after 20,000 characters.
pub fn changelog_text() -> String {
    let mut text = EMBEDDED_CHANGELOG.to_string();
    if text.chars().count() > CHANGELOG_MAX_CHARS {
        text = text.chars().take(CHANGELOG_MAX_CHARS).collect::<String>() + "\n\n...(truncated)...";
    }
    text
}

/// Why `/export` has nothing to write yet; `None` when the session file has messages.
pub fn export_blocker(session_file: Option<&Path>, message_count: u64) -> Option<&'static str> {
    let Some(file) = session_file.filter(|file| message_count > 0 && file.exists()) else {
        return Some("Nothing to export yet (no session messages). Send a prompt first.");
    };
    match std::fs::read_to_string(file) {
        Ok(raw) if raw.trim().is_empty() => Some("Nothing to export yet (empty session file). Send a prompt first."),
        Ok(_) => None,
        Err(_) => Some("Couldn't read session file for export. Try sending a prompt first."),
    }
}

/// `pi-session-<id>.html`, with characters other than ASCII letters, digits, `_`, and `-` as `_`.
pub fn export_file_name(session_id: &str) -> String {
    let safe_id: String =
        session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("pi-session-{safe_id}.html")
}

/// Run `/session`, `/changelog`, `/export`, or `/name` on `session`; `None` for another name.
pub fn run_session_command(session: &AgentSession, name: &str, args: &str) -> Option<String> {
    Some(match name {
        "session" => format_session_stats(&session.session_stats().to_json()),
        "changelog" => changelog_text(),
        "export" => {
            let file = session.session_file();
            let count = session.with_state(|state| state.messages.len()) as u64;
            if let Some(blocker) = export_blocker(file.as_deref(), count) {
                return Some(blocker.to_string());
            }
            let path = session.cwd().join(export_file_name(&session.session_id()));
            match session.export_to_html(Some(&path.display().to_string())) {
                Ok(path) => format!("Session exported: {}", path.display()),
                Err(error) => format!("Export failed: {error:#}"),
            }
        }
        "name" => {
            let name = parse_command_args(args).join(" ").trim().to_string();
            if name.is_empty() {
                "Usage: /name <name>".to_string()
            } else {
                match session.set_session_name(&name) {
                    Ok(()) => format!("Session name set: {name}"),
                    Err(error) => format!("Failed to set session name: {error:#}"),
                }
            }
        }
        _ => return None,
    })
}
