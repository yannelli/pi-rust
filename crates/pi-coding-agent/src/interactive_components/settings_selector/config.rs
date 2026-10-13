//! Data and callbacks of the settings selector (upstream `SettingsConfig` and
//! `SettingsCallbacks`).

use std::collections::BTreeMap;

use pi_agent::QueueMode;
use pi_ai::{Model, ThinkingLevel, Transport};

use crate::settings::QuietStartup;
use crate::settings::defaults::{DEFAULT_HTTP_IDLE_TIMEOUT_MS, DEFAULT_THINKING_LEVEL};
use crate::settings::schema::{
    CacheWarmingMode, DoubleEscapeAction, FullscreenExitOutput, FullscreenScrollbar, MermaidRenderingMode,
    TreeFilterMode, TuiMode, WarningSettings,
};
use crate::theme::{SYSTEM_THEME_NAME, TerminalTheme};

/// HTTP idle timeout choices of the selector, with their labels (upstream `HTTP_IDLE_TIMEOUT_CHOICES`).
pub const HTTP_IDLE_TIMEOUT_CHOICES: [(&str, u64); 5] =
    [("30 sec", 30_000), ("1 min", 60_000), ("2 min", 120_000), ("5 min", 300_000), ("disabled", 0)];

/// The choice label of a timeout, or `<seconds> sec`.
pub fn format_http_idle_timeout_ms(timeout_ms: u64) -> String {
    if let Some((label, _)) = HTTP_IDLE_TIMEOUT_CHOICES.iter().find(|(_, ms)| *ms == timeout_ms) {
        return (*label).to_string();
    }
    if timeout_ms % 1000 == 0 {
        format!("{} sec", timeout_ms / 1000)
    } else {
        format!("{} sec", timeout_ms as f64 / 1000.0)
    }
}

/// `defaultProjectTrust`: what happens when nothing else decides project trust.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefaultProjectTrust {
    #[default]
    Ask,
    Always,
    Never,
}

impl DefaultProjectTrust {
    pub const ALL: [Self; 3] = [Self::Ask, Self::Always, Self::Never];

    /// The settings value: `ask`, `always`, or `never`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// The label shown in the selector.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "Ask",
            Self::Always => "Always trust",
            Self::Never => "Never trust",
        }
    }

    /// Parses a settings value; anything else is `Ask`.
    pub fn from_setting(value: &str) -> Self {
        Self::ALL.into_iter().find(|trust| trust.as_str() == value).unwrap_or_default()
    }
}

/// Current values shown by the settings selector.
#[derive(Clone, Debug)]
pub struct SettingsConfig {
    pub auto_compact: bool,
    /// `provider/modelId`, or `not set`.
    pub default_model: String,
    pub current_model: Option<Model>,
    pub available_default_models: Vec<Model>,
    pub show_images: bool,
    pub image_width_cells: u64,
    pub auto_resize_images: bool,
    pub block_images: bool,
    pub enable_skill_commands: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub transport: Transport,
    pub http_idle_timeout_ms: u64,
    pub cache_warming_mode: CacheWarmingMode,
    /// The global default thinking level.
    pub thinking_level: ThinkingLevel,
    pub available_thinking_levels: Vec<ThinkingLevel>,
    /// Per-model overrides keyed by `provider/modelId`.
    pub model_thinking_levels: BTreeMap<String, ThinkingLevel>,
    /// The theme setting: a name or a `light/dark` pair.
    pub current_theme: String,
    pub terminal_theme: TerminalTheme,
    pub available_themes: Vec<String>,
    pub hide_thinking_block: bool,
    pub mermaid_rendering_mode: MermaidRenderingMode,
    pub show_cache_miss_notices: bool,
    pub collapse_changelog: bool,
    pub enable_install_telemetry: bool,
    pub double_escape_action: DoubleEscapeAction,
    pub tree_filter_mode: TreeFilterMode,
    pub show_hardware_cursor: bool,
    pub editor_padding_x: u64,
    /// `0` or `1`.
    pub output_pad: u8,
    pub autocomplete_max_visible: u64,
    pub quiet_startup: QuietStartup,
    pub default_project_trust: DefaultProjectTrust,
    pub clear_on_shrink: bool,
    pub show_terminal_progress: bool,
    pub tui_mode: TuiMode,
    pub fullscreen_exit_output: FullscreenExitOutput,
    pub fullscreen_scrollbar: FullscreenScrollbar,
    pub fullscreen_copy_on_select: bool,
    /// Lines per wheel event; `None` is `auto`.
    pub fullscreen_wheel_scroll_lines: Option<u64>,
    pub warnings: WarningSettings,
}

/// Upstream `SETTINGS_DEFAULTS`, with no models and no themes.
impl Default for SettingsConfig {
    fn default() -> Self {
        Self {
            auto_compact: true,
            default_model: "not set".into(),
            current_model: None,
            available_default_models: Vec::new(),
            show_images: true,
            image_width_cells: 60,
            auto_resize_images: true,
            block_images: false,
            enable_skill_commands: true,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::OneAtATime,
            transport: Transport::Auto,
            http_idle_timeout_ms: DEFAULT_HTTP_IDLE_TIMEOUT_MS,
            cache_warming_mode: CacheWarmingMode::Streaming,
            thinking_level: DEFAULT_THINKING_LEVEL,
            available_thinking_levels: ThinkingLevel::ALL.to_vec(),
            model_thinking_levels: BTreeMap::new(),
            current_theme: SYSTEM_THEME_NAME.into(),
            terminal_theme: TerminalTheme::Dark,
            available_themes: Vec::new(),
            hide_thinking_block: false,
            mermaid_rendering_mode: MermaidRenderingMode::Streaming,
            show_cache_miss_notices: false,
            collapse_changelog: false,
            enable_install_telemetry: true,
            double_escape_action: DoubleEscapeAction::Tree,
            tree_filter_mode: TreeFilterMode::Default,
            show_hardware_cursor: false,
            editor_padding_x: 0,
            output_pad: 1,
            autocomplete_max_visible: 5,
            quiet_startup: QuietStartup::Off,
            default_project_trust: DefaultProjectTrust::Ask,
            clear_on_shrink: false,
            show_terminal_progress: false,
            tui_mode: TuiMode::Fullscreen,
            fullscreen_exit_output: FullscreenExitOutput::Transcript,
            fullscreen_scrollbar: FullscreenScrollbar::Auto,
            fullscreen_copy_on_select: true,
            fullscreen_wheel_scroll_lines: None,
            warnings: WarningSettings::default(),
        }
    }
}

/// A change callback.
pub type OnChange<T> = Box<dyn FnMut(T)>;
/// A callback receiving text, such as a theme setting.
pub type OnText = Box<dyn FnMut(&str)>;
/// Receives a provider, a model id, and a thinking level.
pub type OnModelLevel = Box<dyn FnMut(&str, &str, ThinkingLevel)>;
/// Receives a provider and a model id.
pub type OnModel = Box<dyn FnMut(&str, &str)>;

/// Called when the user changes a setting. They run on the UI thread while the selector is
/// borrowed; use [`super::SettingsSelector::value_updater`] to correct a shown value.
pub struct SettingsCallbacks {
    pub on_auto_compact_change: OnChange<bool>,
    pub on_show_images_change: OnChange<bool>,
    pub on_image_width_cells_change: OnChange<u64>,
    pub on_auto_resize_images_change: OnChange<bool>,
    pub on_block_images_change: OnChange<bool>,
    pub on_enable_skill_commands_change: OnChange<bool>,
    pub on_steering_mode_change: OnChange<QueueMode>,
    pub on_follow_up_mode_change: OnChange<QueueMode>,
    pub on_transport_change: OnChange<Transport>,
    pub on_http_idle_timeout_ms_change: OnChange<u64>,
    pub on_cache_warming_mode_change: OnChange<CacheWarmingMode>,
    /// Receives the provider, the model id, and the level.
    pub on_model_thinking_level_change: OnModelLevel,
    /// Receives the provider and the model id.
    pub on_model_thinking_level_remove: OnModel,
    /// Receives the new theme setting.
    pub on_theme_change: OnText,
    /// Receives a theme setting to show while browsing; the host routes it to
    /// `InteractiveThemeController::preview`.
    pub on_theme_preview: Option<OnText>,
    pub on_hide_thinking_block_change: OnChange<bool>,
    pub on_mermaid_rendering_mode_change: OnChange<MermaidRenderingMode>,
    pub on_show_cache_miss_notices_change: OnChange<bool>,
    pub on_collapse_changelog_change: OnChange<bool>,
    pub on_enable_install_telemetry_change: OnChange<bool>,
    pub on_double_escape_action_change: OnChange<DoubleEscapeAction>,
    pub on_tree_filter_mode_change: OnChange<TreeFilterMode>,
    pub on_show_hardware_cursor_change: OnChange<bool>,
    pub on_editor_padding_x_change: OnChange<u64>,
    pub on_output_pad_change: OnChange<u8>,
    pub on_autocomplete_max_visible_change: OnChange<u64>,
    pub on_quiet_startup_change: OnChange<QuietStartup>,
    pub on_default_project_trust_change: OnChange<DefaultProjectTrust>,
    pub on_clear_on_shrink_change: OnChange<bool>,
    pub on_show_terminal_progress_change: OnChange<bool>,
    pub on_tui_mode_change: OnChange<TuiMode>,
    pub on_fullscreen_exit_output_change: OnChange<FullscreenExitOutput>,
    pub on_fullscreen_scrollbar_change: OnChange<FullscreenScrollbar>,
    pub on_fullscreen_copy_on_select_change: OnChange<bool>,
    /// `None` is `auto`.
    pub on_fullscreen_wheel_scroll_lines_change: OnChange<Option<u64>>,
    pub on_warnings_change: Box<dyn FnMut(&WarningSettings)>,
    pub on_cancel: Box<dyn FnMut()>,
}

fn ignore<T>() -> OnChange<T> {
    Box::new(|_| {})
}

/// Callbacks that do nothing; set the ones the host needs.
impl Default for SettingsCallbacks {
    fn default() -> Self {
        Self {
            on_auto_compact_change: ignore(),
            on_show_images_change: ignore(),
            on_image_width_cells_change: ignore(),
            on_auto_resize_images_change: ignore(),
            on_block_images_change: ignore(),
            on_enable_skill_commands_change: ignore(),
            on_steering_mode_change: ignore(),
            on_follow_up_mode_change: ignore(),
            on_transport_change: ignore(),
            on_http_idle_timeout_ms_change: ignore(),
            on_cache_warming_mode_change: ignore(),
            on_model_thinking_level_change: Box::new(|_, _, _| {}),
            on_model_thinking_level_remove: Box::new(|_, _| {}),
            on_theme_change: Box::new(|_| {}),
            on_theme_preview: None,
            on_hide_thinking_block_change: ignore(),
            on_mermaid_rendering_mode_change: ignore(),
            on_show_cache_miss_notices_change: ignore(),
            on_collapse_changelog_change: ignore(),
            on_enable_install_telemetry_change: ignore(),
            on_double_escape_action_change: ignore(),
            on_tree_filter_mode_change: ignore(),
            on_show_hardware_cursor_change: ignore(),
            on_editor_padding_x_change: ignore(),
            on_output_pad_change: ignore(),
            on_autocomplete_max_visible_change: ignore(),
            on_quiet_startup_change: ignore(),
            on_default_project_trust_change: ignore(),
            on_clear_on_shrink_change: ignore(),
            on_show_terminal_progress_change: ignore(),
            on_tui_mode_change: ignore(),
            on_fullscreen_exit_output_change: ignore(),
            on_fullscreen_scrollbar_change: ignore(),
            on_fullscreen_copy_on_select_change: ignore(),
            on_fullscreen_wheel_scroll_lines_change: ignore(),
            on_warnings_change: Box::new(|_| {}),
            on_cancel: Box::new(|| {}),
        }
    }
}
