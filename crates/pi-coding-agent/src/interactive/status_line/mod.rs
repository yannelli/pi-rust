//! `statusLine` (pi-rust extension): a custom footer status line. `builtin` draws configurable
//! segments natively; `command` runs a shell command with session JSON on stdin and shows what
//! it prints, like Claude Code's `statusLine`. `claude`, `cursor`, and `codex` reuse the status
//! line that those agents' settings define.

mod builtin;
mod chips;
mod command;
mod editor;
mod editor_items;
mod external;
mod layout_editor;
mod segments;
mod slash;
mod themes;

use std::path::{Path, PathBuf};
use std::time::Duration;

use pi_ai::ThinkingLevel;
use serde_json::{Value, json};

use crate::compaction::ContextUsage;
use crate::session::usage::UsageTotals;
use crate::settings::schema::StatusLineSettings;

pub use builtin::{BuiltinStatusLine, preset_names};
pub use chips::Chip;
pub use command::{CommandConfig, CommandStatusLine};
pub use segments::{Icons, Segment};
pub use themes::theme_names;

/// Whether the agent works, compacts, or waits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunState {
    #[default]
    Idle,
    Working,
    Compacting,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Idle => "idle",
            RunState::Working => "working",
            RunState::Compacting => "compacting",
        }
    }
}

/// The model fields a status line shows.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusModel {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
}

/// What the footer knows about the session, rebuilt on every render.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusSnapshot {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    /// The name of the git repository folder.
    pub project: Option<String>,
    pub branch: Option<String>,
    pub session_id: String,
    pub session_name: Option<String>,
    pub session_file: Option<PathBuf>,
    pub totals: UsageTotals,
    pub cache_hit_rate: Option<f64>,
    pub context: Option<ContextUsage>,
    pub context_window: Option<u64>,
    pub auto_compact: bool,
    pub model: Option<StatusModel>,
    pub thinking: ThinkingLevel,
    pub routed: Option<(String, Option<ThinkingLevel>)>,
    pub subscription: bool,
    pub run_state: RunState,
}

impl StatusSnapshot {
    /// Context use in percent, when known.
    pub fn context_percent(&self) -> Option<f64> {
        self.context.as_ref().and_then(|usage| usage.percent)
    }

    pub fn context_headroom(&self) -> Option<u64> {
        Some(self.context_window?.saturating_sub(self.context.as_ref()?.tokens?))
    }
}

/// A parsed `statusLine` setting.
#[derive(Clone, Debug, PartialEq)]
pub enum StatusLineConfig {
    Builtin(BuiltinStatusLine),
    Command(CommandConfig),
}

impl StatusLineConfig {
    /// `None` when the setting is absent or `off`, the type is unknown, or no command is found.
    pub fn from_settings(settings: &StatusLineSettings, shell_path: Option<&Path>) -> Option<Self> {
        let padding = |value: Option<f64>| {
            value.filter(|value| value.is_finite() && *value >= 0.0).unwrap_or(0.0).min(40.0) as usize
        };
        let refresh = |value: Option<f64>| {
            value.filter(|seconds| seconds.is_finite() && *seconds >= 1.0).map(Duration::from_secs_f64)
        };
        let command = |command: String, pad: usize, refresh: Option<Duration>| {
            Self::Command(CommandConfig {
                command,
                padding: pad,
                refresh,
                shell_path: shell_path.map(Path::to_path_buf),
            })
        };
        let home = crate::config::home_dir();
        let pad = padding(settings.padding);
        match settings.kind.as_deref() {
            Some("builtin") | None => Some(Self::Builtin(BuiltinStatusLine::from_settings(settings, pad))),
            Some("command") => {
                let text = settings.command.clone().filter(|command| !command.trim().is_empty())?;
                Some(command(text, pad, refresh(settings.refresh_interval)))
            }
            Some(kind @ ("claude" | "cursor")) => {
                let path = if kind == "claude" {
                    external::claude_settings_path(&home)
                } else {
                    external::cursor_config_path(&home)
                };
                let found = external::read_command(&path)?;
                let pad = settings.padding.map_or(padding(found.padding), |_| pad);
                Some(command(found.command, pad, refresh(settings.refresh_interval.or(found.refresh_interval))))
            }
            Some("codex") => {
                let items = external::read_codex_items(&external::codex_config_path(&home)).unwrap_or_default();
                Some(Self::Builtin(BuiltinStatusLine::from_codex(&items, pad)))
            }
            Some(_) => None,
        }
    }
}

/// The JSON a `command` status line reads on stdin. Field names follow Claude Code's status
/// line input where pi has the same data, so existing scripts keep working.
pub fn status_payload(snapshot: &StatusSnapshot) -> Value {
    let cwd = snapshot.cwd.display().to_string();
    let percent = snapshot.context_percent();
    let model = snapshot.model.as_ref().map(|model| {
        json!({ "id": model.id, "display_name": model.name, "provider": model.provider, "reasoning": model.reasoning })
    });
    let thinking =
        snapshot.model.as_ref().is_some_and(|model| model.reasoning).then(
            || json!({ "enabled": snapshot.thinking != ThinkingLevel::Off, "level": snapshot.thinking.as_str() }),
        );
    let routed =
        snapshot.routed.as_ref().map(|(id, level)| json!({ "id": id, "thinking_level": level.map(|l| l.as_str()) }));
    let totals = &snapshot.totals;
    json!({
        "hook_event_name": "Status",
        "session_id": snapshot.session_id,
        "session_name": snapshot.session_name,
        "transcript_path": snapshot.session_file.as_ref().map(|path| path.display().to_string()),
        "cwd": cwd,
        "workspace": { "current_dir": cwd, "project_dir": cwd },
        "git": { "branch": snapshot.branch, "project": snapshot.project },
        "run_state": snapshot.run_state.as_str(),
        "model": model,
        "routed_model": routed,
        "thinking": thinking,
        "version": env!("CARGO_PKG_VERSION"),
        "cost": { "total_cost_usd": totals.cost, "subscription": snapshot.subscription },
        "tokens": {
            "input": totals.input,
            "output": totals.output,
            "cache_read": totals.cache_read,
            "cache_write": totals.cache_write,
            "cache_hit_percentage": snapshot.cache_hit_rate,
        },
        "context_window": {
            "total_input_tokens": snapshot.context.as_ref().and_then(|usage| usage.tokens),
            "total_output_tokens": totals.output,
            "context_window_size": snapshot.context_window,
            "used_percentage": percent,
            "remaining_percentage": percent.map(|percent| (100.0 - percent).max(0.0)),
            "remaining_tokens": snapshot.context_headroom(),
            "auto_compact": snapshot.auto_compact,
        },
    })
}

/// The global `statusLine` after `/statusline <choice>`. A preset clears `chip`, `style`,
/// `theme`, `icons`, `segments`, and `separator`; `command`, `colors`, and `padding` stay.
pub fn status_line_choice(current: Option<Value>, choice: &str) -> Result<Value, String> {
    let mut object = match current {
        Some(Value::Object(object)) => object,
        _ => serde_json::Map::new(),
    };
    if preset_names().any(|preset| preset == choice) {
        for key in ["chip", "style", "theme", "icons", "segments", "separator", "layout"] {
            object.remove(key);
        }
        object.insert("type".into(), json!("builtin"));
        object.insert("preset".into(), json!(choice));
        return Ok(Value::Object(object));
    }
    match choice {
        "command" => {
            if !object.get("command").and_then(Value::as_str).is_some_and(|command| !command.trim().is_empty()) {
                return Err("Set statusLine.command in ~/.pi/agent/settings.json first.".into());
            }
            object.insert("type".into(), json!("command"));
        }
        "from-claude" | "from-cursor" | "from-codex" => {
            object.insert("type".into(), json!(&choice["from-".len()..]));
        }
        "off" => {
            object.insert("type".into(), json!("off"));
        }
        _ => return Err(format!("Unknown status line \"{choice}\". Use {}.", status_line_choices())),
    }
    Ok(Value::Object(object))
}

/// Everything `/statusline` accepts.
pub fn status_line_choices() -> String {
    let presets: Vec<&str> = preset_names().collect();
    format!("{}, command, from-claude, from-cursor, from-codex, or off", presets.join(", "))
}

/// A short name for the active `statusLine`, such as `pills` or `from-claude`.
pub fn status_line_name(settings: Option<&StatusLineSettings>) -> String {
    match settings.map(|settings| (settings.kind.as_deref(), settings.preset.as_deref())) {
        None => "default footer".into(),
        Some((Some("builtin") | None, preset)) => preset.unwrap_or("minimal").into(),
        Some((Some("command"), _)) => "command".into(),
        Some((Some(kind @ ("claude" | "cursor" | "codex")), _)) => format!("from-{kind}"),
        Some((Some(_), _)) => "off".into(),
    }
}

#[cfg(test)]
mod tests;
