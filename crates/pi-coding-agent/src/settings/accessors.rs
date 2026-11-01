//! Typed getters with upstream defaults, and setters that persist to the global (or, for
//! resource lists, project) settings file.

use std::collections::BTreeMap;
use std::path::PathBuf;

use pi_agent::QueueMode;
use pi_ai::{Model, ThinkingLevel, Transport};
use serde_json::{Value, json};

use super::defaults::*;
use super::manager::SettingsManager;
use super::schema::*;
use super::storage::SettingsScope;
use crate::config::expand_tilde;

/// `quietStartup`: hide all startup output, or keep only the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuietStartup {
    #[default]
    Off,
    On,
    Header,
}

/// Agent-level retry policy (`retry.*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryConfig {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_agent_delay_ms: u64,
}

/// Provider request settings (`retry.provider.*`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProviderRetryConfig {
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: u64,
}

/// Effective compaction settings for one model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub reserve_tokens: u64,
    pub keep_recent_tokens: u64,
}

/// Resource list kinds that both settings scopes can contribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Extensions,
    Skills,
    Prompts,
    Themes,
}

/// `number >= 0` or a numeric string gives milliseconds; `"disabled"` gives 0.
pub fn parse_timeout_setting(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) if text.trim().eq_ignore_ascii_case("disabled") => Some(0),
        Value::String(text) if !text.trim().is_empty() => {
            text.trim().parse::<f64>().ok().and_then(|n| parse_timeout_setting(&json!(n)))
        }
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite() && *n >= 0.0).map(|n| n.floor() as u64),
        _ => None,
    }
}

fn non_negative(value: Option<f64>) -> Option<u64> {
    value.filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64)
}

macro_rules! flag {
    ($get:ident, $set:ident, $key:literal, $field:ident, $default:expr) => {
        pub fn $get(&self) -> bool {
            self.state().merged.$field.unwrap_or($default)
        }

        pub fn $set(&self, value: bool) {
            self.set_global_value($key, Some(Value::Bool(value)));
        }
    };
}

macro_rules! choice {
    ($get:ident, $set:ident, $key:literal, $field:ident, $ty:ty) => {
        pub fn $get(&self) -> $ty {
            self.state().merged.$field.unwrap_or_default()
        }

        pub fn $set(&self, value: $ty) {
            self.set_global_value($key, serde_json::to_value(value).ok());
        }
    };
}

macro_rules! nested_flag {
    ($get:ident, $set:ident, $key:literal, $nested:literal, $field:ident, $inner:ident, $default:expr) => {
        pub fn $get(&self) -> bool {
            self.state().merged.$field.as_ref().and_then(|value| value.$inner).unwrap_or($default)
        }

        pub fn $set(&self, value: bool) {
            self.set_global_nested($key, $nested, Some(Value::Bool(value)));
        }
    };
}

impl SettingsManager {
    flag!(hide_thinking_block, set_hide_thinking_block, "hideThinkingBlock", hide_thinking_block, false);
    flag!(show_cache_miss_notices, set_show_cache_miss_notices, "showCacheMissNotices", show_cache_miss_notices, false);
    flag!(collapse_changelog, set_collapse_changelog, "collapseChangelog", collapse_changelog, false);
    flag!(
        enable_install_telemetry,
        set_enable_install_telemetry,
        "enableInstallTelemetry",
        enable_install_telemetry,
        true
    );
    flag!(enable_skill_commands, set_enable_skill_commands, "enableSkillCommands", enable_skill_commands, true);
    flag!(
        fullscreen_copy_on_select,
        set_fullscreen_copy_on_select,
        "fullscreenCopyOnSelect",
        fullscreen_copy_on_select,
        true
    );
    choice!(steering_mode, set_steering_mode, "steeringMode", steering_mode, QueueMode);
    choice!(follow_up_mode, set_follow_up_mode, "followUpMode", follow_up_mode, QueueMode);
    choice!(
        double_escape_action,
        set_double_escape_action,
        "doubleEscapeAction",
        double_escape_action,
        DoubleEscapeAction
    );
    choice!(tree_filter_mode, set_tree_filter_mode, "treeFilterMode", tree_filter_mode, TreeFilterMode);
    choice!(tui_mode, set_tui_mode, "tuiMode", tui_mode, TuiMode);
    choice!(extension_runtime, set_extension_runtime, "extensionRuntime", extension_runtime, ExtensionRuntime);
    choice!(
        fullscreen_exit_output,
        set_fullscreen_exit_output,
        "fullscreenExitOutput",
        fullscreen_exit_output,
        FullscreenExitOutput
    );
    choice!(
        fullscreen_scrollbar,
        set_fullscreen_scrollbar,
        "fullscreenScrollbar",
        fullscreen_scrollbar,
        FullscreenScrollbar
    );
    nested_flag!(show_images, set_show_images, "terminal", "showImages", terminal, show_images, true);
    nested_flag!(
        show_terminal_progress,
        set_show_terminal_progress,
        "terminal",
        "showTerminalProgress",
        terminal,
        show_terminal_progress,
        false
    );
    nested_flag!(image_auto_resize, set_image_auto_resize, "images", "autoResize", images, auto_resize, true);
    nested_flag!(block_images, set_block_images, "images", "blockImages", images, block_images, false);

    pub fn last_changelog_version(&self) -> Option<String> {
        self.state().merged.last_changelog_version.clone()
    }

    pub fn set_last_changelog_version(&self, version: &str) {
        self.set_global_value("lastChangelogVersion", Some(json!(version)));
    }

    /// `sessionDir` with a leading `~` expanded.
    pub fn session_dir(&self) -> Option<PathBuf> {
        self.state().merged.session_dir.as_deref().filter(|dir| !dir.is_empty()).map(expand_tilde)
    }

    pub fn default_provider(&self) -> Option<String> {
        self.state().merged.default_provider.clone()
    }

    pub fn default_model(&self) -> Option<String> {
        self.state().merged.default_model.clone()
    }

    pub fn set_default_provider(&self, provider: &str) {
        self.set_global_value("defaultProvider", Some(json!(provider)));
    }

    pub fn set_default_model(&self, model_id: &str) {
        self.set_global_value("defaultModel", Some(json!(model_id)));
    }

    pub fn set_default_model_and_provider(&self, provider: &str, model_id: &str) {
        self.set_default_provider(provider);
        self.set_default_model(model_id);
    }

    /// The `theme` setting as written, including `light/dark` pairs.
    pub fn theme_setting(&self) -> Option<String> {
        self.state().merged.theme.clone()
    }

    /// A fixed theme name; `None` for automatic `light/dark` pairs.
    pub fn theme(&self) -> Option<String> {
        self.theme_setting().filter(|theme| !theme.contains('/'))
    }

    pub fn set_theme(&self, theme: &str) {
        self.set_global_value("theme", Some(json!(theme)));
    }

    pub fn default_thinking_level(&self) -> Option<ThinkingLevel> {
        self.state().merged.default_thinking_level
    }

    pub fn set_default_thinking_level(&self, level: ThinkingLevel) {
        self.set_global_value("defaultThinkingLevel", Some(json!(level)));
    }

    pub fn fast_mode(&self) -> bool {
        self.state().merged.fast_mode.unwrap_or(false)
    }

    pub fn set_fast_mode(&self, enabled: bool) {
        self.set_global_value("fastMode", Some(json!(enabled)));
    }

    pub fn model_thinking_level(&self, provider: &str, model_id: &str) -> Option<ThinkingLevel> {
        self.state().merged.model_thinking_levels.as_ref()?.get(&format!("{provider}/{model_id}")).copied()
    }

    pub fn all_model_thinking_levels(&self) -> BTreeMap<String, ThinkingLevel> {
        self.state().merged.model_thinking_levels.clone().unwrap_or_default()
    }

    pub fn set_model_thinking_level(&self, provider: &str, model_id: &str, level: ThinkingLevel) {
        self.set_global_nested("modelThinkingLevels", &format!("{provider}/{model_id}"), Some(json!(level)));
    }

    pub fn remove_model_thinking_level(&self, provider: &str, model_id: &str) {
        let _ = self.write(SettingsScope::Global, "modelThinkingLevels", None, |map| {
            if let Some(Value::Object(levels)) = map.get_mut("modelThinkingLevels") {
                levels.shift_remove(&format!("{provider}/{model_id}"));
                if levels.is_empty() {
                    map.shift_remove("modelThinkingLevels");
                }
            }
        });
    }

    pub fn transport(&self) -> Transport {
        self.state().merged.transport.unwrap_or(Transport::Auto)
    }

    pub fn set_transport(&self, transport: Transport) {
        self.set_global_value("transport", serde_json::to_value(transport).ok());
    }

    pub fn session_share_destination(&self) -> SessionShareDestination {
        self.state().merged.session_share_destination.unwrap_or_default()
    }

    pub fn set_session_share_destination(&self, destination: SessionShareDestination) {
        self.set_global_value("sessionShareDestination", serde_json::to_value(destination).ok());
    }

    pub fn compaction_enabled(&self) -> bool {
        self.state().merged.compaction.as_ref().and_then(|c| c.enabled).unwrap_or(COMPACTION_ENABLED)
    }

    pub fn set_compaction_enabled(&self, enabled: bool) {
        self.set_global_nested("compaction", "enabled", Some(json!(enabled)));
    }

    /// Model override (`provider/id`), then the ordinary setting, then the default.
    pub fn compaction_settings(&self, model: Option<&Model>) -> CompactionConfig {
        let state = self.state();
        let compaction = state.merged.compaction.clone().unwrap_or_default();
        let key = model.map(|model| format!("{}/{}", model.provider, model.id));
        let entry = key.and_then(|key| compaction.model_overrides.as_ref()?.get(&key).cloned()).unwrap_or_default();
        CompactionConfig {
            enabled: compaction.enabled.unwrap_or(COMPACTION_ENABLED),
            reserve_tokens: entry.reserve_tokens.or(compaction.reserve_tokens).unwrap_or(COMPACTION_RESERVE_TOKENS),
            keep_recent_tokens: entry
                .keep_recent_tokens
                .or(compaction.keep_recent_tokens)
                .unwrap_or(COMPACTION_KEEP_RECENT_TOKENS),
        }
    }

    /// `(reserveTokens, skipPrompt)` for branch summaries.
    pub fn branch_summary_settings(&self) -> (u64, bool) {
        let summary = self.state().merged.branch_summary.clone().unwrap_or_default();
        let reserve = non_negative(summary.reserve_tokens).unwrap_or(BRANCH_SUMMARY_RESERVE_TOKENS);
        (reserve, summary.skip_prompt.unwrap_or(false))
    }

    pub fn retry_enabled(&self) -> bool {
        self.retry_settings().enabled
    }

    pub fn set_retry_enabled(&self, enabled: bool) {
        self.set_global_nested("retry", "enabled", Some(json!(enabled)));
    }

    pub fn retry_settings(&self) -> RetryConfig {
        let retry = self.state().merged.retry.clone().unwrap_or_default();
        RetryConfig {
            enabled: retry.enabled.unwrap_or(RETRY_ENABLED),
            max_retries: non_negative(retry.max_retries)
                .map_or(RETRY_MAX_RETRIES, |n| n.min(u64::from(u32::MAX)) as u32),
            base_delay_ms: non_negative(retry.base_delay_ms).unwrap_or(RETRY_BASE_DELAY_MS),
            max_agent_delay_ms: non_negative(retry.max_agent_delay_ms).unwrap_or(RETRY_MAX_AGENT_DELAY_MS),
        }
    }

    pub fn set_retry_max_retries(&self, max_retries: u32) {
        self.set_global_nested("retry", "maxRetries", Some(json!(max_retries)));
    }

    pub fn set_retry_base_delay_ms(&self, delay_ms: u64) {
        self.set_global_nested("retry", "baseDelayMs", Some(json!(delay_ms)));
    }

    pub fn set_retry_max_agent_delay_ms(&self, delay_ms: u64) {
        self.set_global_nested("retry", "maxAgentDelayMs", Some(json!(delay_ms)));
    }

    /// `None` removes `retry.provider.timeoutMs`.
    pub fn set_provider_timeout_ms(&self, timeout_ms: Option<u64>) {
        self.set_global_nested2("retry", "provider", "timeoutMs", timeout_ms.map(|ms| json!(ms)));
    }

    /// `None` removes `retry.provider.maxRetries`.
    pub fn set_provider_max_retries(&self, max_retries: Option<u32>) {
        self.set_global_nested2("retry", "provider", "maxRetries", max_retries.map(|n| json!(n)));
    }

    pub fn set_provider_max_retry_delay_ms(&self, delay_ms: u64) {
        self.set_global_nested2("retry", "provider", "maxRetryDelayMs", Some(json!(delay_ms)));
    }

    /// Global `disabledProviders`; project values are ignored.
    pub fn disabled_providers(&self) -> Vec<String> {
        self.state().global_typed.disabled_providers.clone().unwrap_or_default()
    }

    /// An empty list removes `disabledProviders`.
    pub fn set_disabled_providers(&self, providers: Vec<String>) {
        let value = (!providers.is_empty()).then(|| json!(providers));
        self.set_global_value("disabledProviders", value);
    }

    pub fn provider_retry_settings(&self) -> ProviderRetryConfig {
        let provider = self.state().merged.retry.clone().and_then(|retry| retry.provider).unwrap_or_default();
        ProviderRetryConfig {
            timeout_ms: non_negative(provider.timeout_ms),
            max_retries: non_negative(provider.max_retries).map(|n| n.min(u64::from(u32::MAX)) as u32),
            max_retry_delay_ms: non_negative(provider.max_retry_delay_ms).unwrap_or(PROVIDER_MAX_RETRY_DELAY_MS),
        }
    }

    /// HTTP idle timeout in milliseconds; 0 disables it. Invalid values are an error.
    pub fn http_idle_timeout_ms(&self) -> Result<u64, String> {
        match &self.state().merged.http_idle_timeout_ms {
            None => Ok(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            Some(value) => {
                parse_timeout_setting(value).ok_or_else(|| format!("Invalid httpIdleTimeoutMs setting: {value}"))
            }
        }
    }

    pub fn set_http_idle_timeout_ms(&self, timeout_ms: u64) {
        self.set_global_value("httpIdleTimeoutMs", Some(json!(timeout_ms)));
    }

    pub fn websocket_connect_timeout_ms(&self) -> Result<Option<u64>, String> {
        match &self.state().merged.websocket_connect_timeout_ms {
            None => Ok(None),
            Some(value) => parse_timeout_setting(value)
                .map(Some)
                .ok_or_else(|| format!("Invalid websocketConnectTimeoutMs setting: {value}")),
        }
    }

    /// Global only, because warming costs money.
    pub fn cache_warming_mode(&self) -> CacheWarmingMode {
        self.state().global_typed.cache_warming.unwrap_or_default()
    }

    pub fn set_cache_warming_mode(&self, mode: CacheWarmingMode) {
        self.set_global_value("cacheWarming", serde_json::to_value(mode).ok());
    }

    /// `externalEditor`, then `$VISUAL`, then `$EDITOR`, then `notepad` or `nano`.
    pub fn external_editor_command(&self) -> String {
        let configured = self.state().merged.external_editor.clone().filter(|editor| !editor.trim().is_empty());
        configured
            .or_else(|| std::env::var("VISUAL").ok().filter(|v| !v.is_empty()))
            .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
            .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "nano".into() })
    }

    /// `shellPath` with a leading `~` expanded.
    pub fn shell_path(&self) -> Option<PathBuf> {
        self.state().merged.shell_path.as_deref().filter(|path| !path.is_empty()).map(expand_tilde)
    }

    pub fn set_shell_path(&self, path: Option<&str>) {
        self.set_global_value("shellPath", path.map(|path| json!(path)));
    }

    pub fn shell_command_prefix(&self) -> Option<String> {
        self.state().merged.shell_command_prefix.clone()
    }

    pub fn set_shell_command_prefix(&self, prefix: Option<&str>) {
        self.set_global_value("shellCommandPrefix", prefix.map(|prefix| json!(prefix)));
    }

    pub fn quiet_startup(&self) -> QuietStartup {
        match self.state().merged.quiet_startup {
            Some(Value::Bool(true)) => QuietStartup::On,
            Some(Value::String(ref value)) if value == "header" => QuietStartup::Header,
            _ => QuietStartup::Off,
        }
    }

    pub fn set_quiet_startup(&self, quiet: QuietStartup) {
        let value = match quiet {
            QuietStartup::Off => json!(false),
            QuietStartup::On => json!(true),
            QuietStartup::Header => json!("header"),
        };
        self.set_global_value("quietStartup", Some(value));
    }

    /// `"ask"`, `"always"`, or `"never"` from global settings only.
    pub fn default_project_trust(&self) -> &'static str {
        match self.state().global_typed.default_project_trust.as_deref() {
            Some("always") => "always",
            Some("never") => "never",
            _ => "ask",
        }
    }

    pub fn set_default_project_trust(&self, trust: &str) {
        self.set_global_value("defaultProjectTrust", Some(json!(trust)));
    }

    pub fn npm_command(&self) -> Option<Vec<String>> {
        self.state().merged.npm_command.clone()
    }

    pub fn set_npm_command(&self, command: Option<&[String]>) {
        self.set_global_value("npmCommand", command.map(|command| json!(command)));
    }
}
