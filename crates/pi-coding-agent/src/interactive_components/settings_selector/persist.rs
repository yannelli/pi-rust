//! Settings selector data read from, and callbacks that write to, a [`SettingsManager`].

use std::sync::Arc;

use super::config::{DefaultProjectTrust, SettingsCallbacks, SettingsConfig};
use crate::settings::SettingsManager;
use crate::settings::defaults::{DEFAULT_HTTP_IDLE_TIMEOUT_MS, DEFAULT_THINKING_LEVEL};
use crate::theme::{SYSTEM_THEME_NAME, get_available_themes, get_terminal_theme};

impl SettingsConfig {
    /// Values from settings, the theme registry, and the terminal appearance. The host fills in
    /// session state: `current_model`, `available_default_models`, and runtime values such as
    /// `tui_mode` or a theme selection that differs from the setting.
    pub fn from_settings(settings: &SettingsManager) -> Self {
        let default_model = match (settings.default_provider(), settings.default_model()) {
            (Some(provider), Some(model)) => format!("{provider}/{model}"),
            _ => "not set".into(),
        };
        Self {
            auto_compact: settings.compaction_enabled(),
            default_model,
            show_images: settings.show_images(),
            image_width_cells: settings.image_width_cells(),
            auto_resize_images: settings.image_auto_resize(),
            block_images: settings.block_images(),
            enable_skill_commands: settings.enable_skill_commands(),
            steering_mode: settings.steering_mode(),
            follow_up_mode: settings.follow_up_mode(),
            transport: settings.transport(),
            session_share_destination: settings.session_share_destination(),
            http_idle_timeout_ms: settings.http_idle_timeout_ms().unwrap_or(DEFAULT_HTTP_IDLE_TIMEOUT_MS),
            cache_warming_mode: settings.cache_warming_mode(),
            retry: settings.retry_settings(),
            provider_retry: settings.provider_retry_settings(),
            disabled_providers: settings.disabled_providers(),
            thinking_level: settings.default_thinking_level().unwrap_or(DEFAULT_THINKING_LEVEL),
            model_thinking_levels: settings.all_model_thinking_levels(),
            current_theme: settings.theme_setting().unwrap_or_else(|| SYSTEM_THEME_NAME.into()),
            terminal_theme: get_terminal_theme(),
            available_themes: get_available_themes(),
            hide_thinking_block: settings.hide_thinking_block(),
            mermaid_rendering_mode: settings.mermaid_rendering_mode(),
            show_cache_miss_notices: settings.show_cache_miss_notices(),
            collapse_changelog: settings.collapse_changelog(),
            enable_install_telemetry: settings.enable_install_telemetry(),
            double_escape_action: settings.double_escape_action(),
            tree_filter_mode: settings.tree_filter_mode(),
            show_hardware_cursor: settings.show_hardware_cursor(),
            editor_padding_x: settings.editor_padding_x(),
            output_pad: settings.output_pad(),
            autocomplete_max_visible: settings.autocomplete_max_visible(),
            quiet_startup: settings.quiet_startup(),
            default_project_trust: DefaultProjectTrust::from_setting(settings.default_project_trust()),
            clear_on_shrink: settings.clear_on_shrink(),
            show_terminal_progress: settings.show_terminal_progress(),
            tui_mode: settings.tui_mode(),
            fullscreen_exit_output: settings.fullscreen_exit_output(),
            fullscreen_scrollbar: settings.fullscreen_scrollbar(),
            fullscreen_copy_on_select: settings.fullscreen_copy_on_select(),
            fullscreen_wheel_scroll_lines: settings.fullscreen_wheel_scroll_lines(),
            ..Self::default()
        }
    }
}

impl SettingsCallbacks {
    /// Callbacks that save each change to `settings`. The host replaces the ones that must also
    /// update the running session, and sets `on_cancel` and `on_theme_preview`.
    pub fn persisting(settings: Arc<SettingsManager>) -> Self {
        macro_rules! save {
            ($setter:ident) => {{
                let settings = settings.clone();
                Box::new(move |value| settings.$setter(value))
            }};
        }
        let (change, remove, theme, trust) = (settings.clone(), settings.clone(), settings.clone(), settings.clone());
        Self {
            on_auto_compact_change: save!(set_compaction_enabled),
            on_show_images_change: save!(set_show_images),
            on_image_width_cells_change: save!(set_image_width_cells),
            on_auto_resize_images_change: save!(set_image_auto_resize),
            on_block_images_change: save!(set_block_images),
            on_enable_skill_commands_change: save!(set_enable_skill_commands),
            on_steering_mode_change: save!(set_steering_mode),
            on_follow_up_mode_change: save!(set_follow_up_mode),
            on_transport_change: save!(set_transport),
            on_session_share_destination_change: save!(set_session_share_destination),
            on_http_idle_timeout_ms_change: save!(set_http_idle_timeout_ms),
            on_cache_warming_mode_change: save!(set_cache_warming_mode),
            on_retry_enabled_change: save!(set_retry_enabled),
            on_retry_max_retries_change: save!(set_retry_max_retries),
            on_retry_base_delay_ms_change: save!(set_retry_base_delay_ms),
            on_retry_max_agent_delay_ms_change: save!(set_retry_max_agent_delay_ms),
            on_provider_timeout_ms_change: save!(set_provider_timeout_ms),
            on_provider_max_retries_change: save!(set_provider_max_retries),
            on_provider_max_retry_delay_ms_change: save!(set_provider_max_retry_delay_ms),
            on_disabled_providers_change: save!(set_disabled_providers),
            on_model_thinking_level_change: Box::new(move |provider, model_id, level| {
                change.set_model_thinking_level(provider, model_id, level)
            }),
            on_model_thinking_level_remove: Box::new(move |provider, model_id| {
                remove.remove_model_thinking_level(provider, model_id)
            }),
            on_theme_change: Box::new(move |setting| theme.set_theme(setting)),
            on_theme_preview: None,
            on_hide_thinking_block_change: save!(set_hide_thinking_block),
            on_mermaid_rendering_mode_change: save!(set_mermaid_rendering_mode),
            on_show_cache_miss_notices_change: save!(set_show_cache_miss_notices),
            on_collapse_changelog_change: save!(set_collapse_changelog),
            on_enable_install_telemetry_change: save!(set_enable_install_telemetry),
            on_double_escape_action_change: save!(set_double_escape_action),
            on_tree_filter_mode_change: save!(set_tree_filter_mode),
            on_show_hardware_cursor_change: save!(set_show_hardware_cursor),
            on_editor_padding_x_change: save!(set_editor_padding_x),
            on_output_pad_change: save!(set_output_pad),
            on_autocomplete_max_visible_change: save!(set_autocomplete_max_visible),
            on_quiet_startup_change: save!(set_quiet_startup),
            on_default_project_trust_change: Box::new(move |value| trust.set_default_project_trust(value.as_str())),
            on_clear_on_shrink_change: save!(set_clear_on_shrink),
            on_show_terminal_progress_change: save!(set_show_terminal_progress),
            on_tui_mode_change: save!(set_tui_mode),
            on_fullscreen_exit_output_change: save!(set_fullscreen_exit_output),
            on_fullscreen_scrollbar_change: save!(set_fullscreen_scrollbar),
            on_fullscreen_copy_on_select_change: save!(set_fullscreen_copy_on_select),
            on_fullscreen_wheel_scroll_lines_change: save!(set_fullscreen_wheel_scroll_lines),
            on_cancel: Box::new(|| {}),
        }
    }
}
