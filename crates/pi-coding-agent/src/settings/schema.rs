//! Typed `settings.json` keys (upstream `core/settings-schema.ts`). Every key is optional;
//! accessors on [`super::SettingsManager`] apply the defaults. Keys whose type is a union
//! (for example `quietStartup: true | "header"`) keep the raw JSON value.

use std::collections::BTreeMap;

use pi_agent::QueueMode;
use pi_ai::{ThinkingLevel, Transport};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionModelOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u64>,
    /// Keyed by exact `provider/modelId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<BTreeMap<String, CompactionModelOverride>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSummarySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_prompt: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retries: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderRetrySettings>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_width_cells: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_on_shrink: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_terminal_progress: Option<bool>,
    /// `true`, `false`, or `"auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hyperlinks: Option<Value>,
    /// `"kitty"`, `"iterm2"`, `"auto"`, or `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Value>,
    /// `true`, `false`, or `"auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub true_color: Option<Value>,
    /// pi-rust extension: how to notify when a long run finishes; `auto` by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_on_finish: Option<NotifyOnFinish>,
    /// pi-rust extension: the shortest run in seconds that notifies; 10 by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_min_seconds: Option<f64>,
    /// pi-rust extension: `true`, `false` (the default), or an object of iTerm2 features.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterm2: Option<Value>,
}

/// `terminal.iterm2` (pi-rust extension): the iTerm2 features to use. `true` turns on all of them;
/// in an object, a feature that is not named is on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ITerm2Features {
    pub tab_color: bool,
    pub badge: bool,
    pub user_vars: bool,
    pub attention: bool,
    pub cwd: bool,
}

impl ITerm2Features {
    pub fn from_value(value: Option<&Value>) -> Self {
        let on = |key: &str| match value {
            Some(Value::Bool(enabled)) => *enabled,
            Some(Value::Object(object)) => object.get(key).and_then(Value::as_bool).unwrap_or(true),
            _ => false,
        };
        Self {
            tab_color: on("tabColor"),
            badge: on("badge"),
            user_vars: on("userVars"),
            attention: on("attention"),
            cwd: on("cwd"),
        }
    }

    pub fn any(&self) -> bool {
        self.tab_color || self.badge || self.user_vars || self.attention || self.cwd
    }
}

/// `terminal.notifyOnFinish` (pi-rust extension): `auto` picks OSC 777, OSC 99, OSC 9, or BEL
/// for the terminal, `osc` always sends an OSC notification, and `bell` sends BEL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyOnFinish {
    #[default]
    Auto,
    Off,
    Bell,
    Osc,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_resize: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkingBudgetsSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimal: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub high: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MermaidRenderingMode {
    Off,
    Final,
    #[default]
    Streaming,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_block_indent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mermaid: Option<MermaidRenderingMode>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeMode {
    #[default]
    On,
    Only,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CodemodeMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_budget: Option<f64>,
}

/// `promptHistory` (pi-rust extension): prompts saved in `<agent dir>/prompt-history.jsonl`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptHistorySettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<f64>,
}

/// `statusLine` (pi-rust extension): replaces the footer's path and stats lines. `type` is
/// `"builtin"`, `"command"` (session JSON on stdin, as in Claude Code), `"claude"`, `"cursor"`,
/// `"codex"`, or `"off"`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusLineSettings {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub padding: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_interval: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The chip shape; `style` is its older name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<String>,
    /// Segment names for one line, or a list of lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub separator: Option<String>,
    /// Segment name to a theme color token or `#rrggbb`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colors: Option<BTreeMap<String, String>>,
}

/// A `packages` entry: a source string, or an object with resource filters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PackageSource {
    Source(String),
    Filtered(PackageFilter),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageFilter {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoubleEscapeAction {
    Fork,
    #[default]
    Tree,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TreeFilterMode {
    #[default]
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

/// Where `/share` uploads: `auto` uses Radius when signed in, else a GitHub gist.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionShareDestination {
    #[default]
    Auto,
    Gist,
    Radius,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheWarmingMode {
    Off,
    #[default]
    Streaming,
    Idle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TuiMode {
    Regular,
    #[default]
    Fullscreen,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FullscreenExitOutput {
    #[default]
    Transcript,
    ResumeHint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullscreenScrollbar {
    #[default]
    Auto,
    Always,
    Hidden,
}

/// Every `settings.json` key. `extra` keeps keys this version does not know.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_changelog_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_thinking_level: Option<ThinkingLevel>,
    /// Keyed by `provider/modelId`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_thinking_levels: Option<BTreeMap<String, ThinkingLevel>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<Transport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_share_destination: Option<SessionShareDestination>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steering_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_mode: Option<QueueMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_summary: Option<BranchSummarySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetrySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hide_thinking_block: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cache_miss_notices: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_editor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_path: Option<String>,
    /// `true`, `false`, or `"header"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_startup: Option<Value>,
    /// `"ask"`, `"always"`, or `"never"`; read from global settings only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_project_trust: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command_prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm_command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collapse_changelog: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_install_telemetry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_analytics: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracking_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageSource>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_skill_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<ImageSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_models: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_escape_action: Option<DoubleEscapeAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_filter_mode: Option<TreeFilterMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_budgets: Option<ThinkingBudgetsSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_padding_x: Option<f64>,
    /// `0` or `1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_pad: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete_max_visible: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_history: Option<PromptHistorySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLineSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_hardware_cursor: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub markdown: Option<MarkdownSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codemode: Option<CodemodeSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
    /// Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy: Option<String>,
    /// Milliseconds, or `"disabled"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_idle_timeout_ms: Option<Value>,
    /// Global only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_warming: Option<CacheWarmingMode>,
    /// Milliseconds, or `"disabled"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websocket_connect_timeout_ms: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui_mode: Option<TuiMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_exit_output: Option<FullscreenExitOutput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_scrollbar: Option<FullscreenScrollbar>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_copy_on_select: Option<bool>,
    /// Lines per wheel event (1 to 100), or `"auto"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen_wheel_scroll_lines: Option<Value>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Typed settings from a JSON object. Keys that do not match the schema are dropped and
/// reported as `(key, message)`; unknown keys go to `extra`.
pub fn parse_settings(map: &Map<String, Value>) -> (Settings, Vec<(String, String)>) {
    if let Ok(settings) = serde_json::from_value::<Settings>(Value::Object(map.clone())) {
        return (settings, Vec::new());
    }
    let mut valid = Map::new();
    let mut problems = Vec::new();
    for (key, value) in map {
        let single = Map::from_iter([(key.clone(), value.clone())]);
        match serde_json::from_value::<Settings>(Value::Object(single)) {
            Ok(_) => {
                valid.insert(key.clone(), value.clone());
            }
            Err(error) => {
                let message = error.to_string();
                let message = message.split(" at line ").next().unwrap_or(&message).to_string();
                problems.push((key.clone(), message));
            }
        }
    }
    (serde_json::from_value(Value::Object(valid)).unwrap_or_default(), problems)
}
