//! The settings of the selector, in upstream order, and the dispatch of value changes.

use pi_tui::components::settings_list::{SettingItem, SubmenuFactory};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::super::key_display_text;
use super::config::{
    DefaultProjectTrust, HTTP_IDLE_TIMEOUT_CHOICES, SettingsCallbacks, SettingsConfig, format_http_idle_timeout_ms,
};
use crate::settings::QuietStartup;

/// The serde name of a settings enum, such as `one-at-a-time`.
pub(super) fn enum_str<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => text,
        _ => String::new(),
    }
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(value.to_string())).ok()
}

fn bool_str(value: bool) -> String {
    value.to_string()
}

fn quiet_startup_str(quiet: QuietStartup) -> &'static str {
    match quiet {
        QuietStartup::On => "true",
        QuietStartup::Header => "header",
        QuietStartup::Off => "false",
    }
}

fn item(id: &str, label: &str, description: &str, current: String, values: &[&str]) -> SettingItem {
    SettingItem {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        current_value: current,
        values: Some(values.iter().map(|value| value.to_string()).collect()),
        submenu: None,
    }
}

fn submenu_item(id: &str, label: &str, description: &str, current: String, factory: SubmenuFactory) -> SettingItem {
    SettingItem {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        current_value: current,
        values: None,
        submenu: Some(factory),
    }
}

const BOOL: &[&str] = &["true", "false"];

/// Submenu factories of the items that open one.
pub(super) struct Submenus {
    pub warnings: SubmenuFactory,
    pub model_thinking: SubmenuFactory,
    pub model_thinking_summary: String,
    pub theme: SubmenuFactory,
}

/// Every setting in upstream order; image settings follow auto-compact.
pub(super) fn build_items(config: &SettingsConfig, supports_images: bool, submenus: Submenus) -> Vec<SettingItem> {
    let follow_up_key = key_display_text("app.message.followUp");
    let cycle_thinking_key = key_display_text("app.thinking.cycle");
    let wheel_values = wheel_scroll_values(config.fullscreen_wheel_scroll_lines);
    let wheel_values: Vec<&str> = wheel_values.iter().map(String::as_str).collect();
    let http_values: Vec<&str> = HTTP_IDLE_TIMEOUT_CHOICES.iter().map(|(label, _)| *label).collect();
    let trust_values: Vec<&str> = DefaultProjectTrust::ALL.iter().map(|trust| trust.label()).collect();

    let mut items = vec![item(
        "autocompact",
        "Auto-compact",
        "Automatically compact context when it gets too large",
        bool_str(config.auto_compact),
        BOOL,
    )];
    if supports_images {
        items.push(item(
            "show-images",
            "Show images",
            "Render images inline in terminal",
            bool_str(config.show_images),
            BOOL,
        ));
        items.push(item(
            "image-width-cells",
            "Image width",
            "Preferred inline image width in terminal cells",
            config.image_width_cells.to_string(),
            &["60", "80", "120"],
        ));
    }
    items.extend([
        item(
            "auto-resize-images",
            "Auto-resize images",
            "Resize large images to 2000x2000 max for better model compatibility",
            bool_str(config.auto_resize_images),
            BOOL,
        ),
        item("block-images", "Block images", "Prevent images from being sent to LLM providers", bool_str(config.block_images), BOOL),
        item("skill-commands", "Skill commands", "Register skills as /skill:name commands", bool_str(config.enable_skill_commands), BOOL),
        item(
            "show-hardware-cursor",
            "Show hardware cursor",
            "Use the terminal cursor instead of Pi's drawn cursor",
            bool_str(config.show_hardware_cursor),
            BOOL,
        ),
        item(
            "editor-padding",
            "Editor padding",
            "Horizontal padding for input editor (0-3)",
            config.editor_padding_x.to_string(),
            &["0", "1", "2", "3"],
        ),
        item(
            "output-padding",
            "Output padding",
            "Horizontal padding for messages, tool output, and command output",
            config.output_pad.to_string(),
            &["0", "1"],
        ),
        item(
            "autocomplete-max-visible",
            "Autocomplete max items",
            "Max visible items in autocomplete dropdown (3-20)",
            config.autocomplete_max_visible.to_string(),
            &["3", "5", "7", "10", "15", "20"],
        ),
        item(
            "clear-on-shrink",
            "Clear on shrink",
            "Clear empty rows when content shrinks (may cause flicker)",
            bool_str(config.clear_on_shrink),
            BOOL,
        ),
        item(
            "terminal-progress",
            "Terminal progress",
            "Show OSC 9;4 progress indicators in the terminal tab bar",
            bool_str(config.show_terminal_progress),
            BOOL,
        ),
        item(
            "steering-mode",
            "Steering mode",
            "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
            enum_str(config.steering_mode),
            &["one-at-a-time", "all"],
        ),
        item(
            "follow-up-mode",
            "Follow-up mode",
            &format!("{follow_up_key} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once."),
            enum_str(config.follow_up_mode),
            &["one-at-a-time", "all"],
        ),
        item(
            "transport",
            "Transport",
            "Preferred transport for providers that support multiple transports",
            enum_str(config.transport),
            &["sse", "websocket", "websocket-cached", "auto"],
        ),
        item(
            "http-idle-timeout",
            "HTTP idle timeout",
            "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.",
            format_http_idle_timeout_ms(config.http_idle_timeout_ms),
            &http_values,
        ),
        item(
            "cache-warming-mode",
            "Cache warming",
            "off; streaming while the agent runs; idle also between runs while continuation stays profitable",
            enum_str(config.cache_warming_mode),
            &["off", "streaming", "idle"],
        ),
        item("hide-thinking", "Hide thinking", "Hide thinking blocks in assistant responses", bool_str(config.hide_thinking_block), BOOL),
        item(
            "mermaid-rendering",
            "Mermaid diagrams",
            "Render Mermaid code blocks as Unicode diagrams",
            enum_str(config.mermaid_rendering_mode),
            &["off", "final", "streaming"],
        ),
        item(
            "cache-miss-notices",
            "Cache miss notices",
            "Show transcript notices for cache costs and provider recovery diagnostics",
            bool_str(config.show_cache_miss_notices),
            BOOL,
        ),
        item(
            "collapse-changelog",
            "Collapse changelog",
            "Show condensed changelog after updates",
            bool_str(config.collapse_changelog),
            BOOL,
        ),
        item(
            "quiet-startup",
            "Quiet startup",
            "Disable verbose printing at startup (header: keep only the startup header)",
            quiet_startup_str(config.quiet_startup).into(),
            &["true", "header", "false"],
        ),
        item(
            "install-telemetry",
            "Install telemetry",
            "Send an anonymous version/update ping after changelog-detected updates",
            bool_str(config.enable_install_telemetry),
            BOOL,
        ),
        item(
            "default-project-trust",
            "Default project trust",
            "Fallback behavior when no extension or saved trust decision decides project trust",
            config.default_project_trust.label().into(),
            &trust_values,
        ),
        item(
            "double-escape-action",
            "Double-escape action",
            "Action when pressing Escape twice with empty editor",
            enum_str(config.double_escape_action),
            &["tree", "fork", "none"],
        ),
        item(
            "tree-filter-mode",
            "Tree filter mode",
            "Default filter when opening /tree",
            enum_str(config.tree_filter_mode),
            &["default", "no-tools", "user-only", "labeled-only", "all"],
        ),
        submenu_item("warnings", "Warnings", "Enable or disable individual warnings", "configure".into(), submenus.warnings),
        submenu_item(
            "model-thinking",
            "Default thinking level per model",
            &format!("Override the default thinking level for specific models. {cycle_thinking_key} cycles in-session."),
            submenus.model_thinking_summary,
            submenus.model_thinking,
        ),
        item(
            "tui-mode",
            "TUI mode",
            "Interface layout; regular mode uses the terminal's normal scrollback",
            enum_str(config.tui_mode),
            &["regular", "fullscreen"],
        ),
        item(
            "fullscreen-exit-output",
            "Fullscreen exit output",
            "Print the transcript or only a session resume hint when exiting fullscreen mode",
            enum_str(config.fullscreen_exit_output),
            &["transcript", "resume-hint"],
        ),
        item(
            "fullscreen-scrollbar",
            "Fullscreen scrollbar",
            "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
            enum_str(config.fullscreen_scrollbar),
            &["auto", "always", "hidden"],
        ),
        item(
            "fullscreen-copy-on-select",
            "Fullscreen copy on select",
            "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
            bool_str(config.fullscreen_copy_on_select),
            BOOL,
        ),
        item(
            "fullscreen-wheel-scroll-lines",
            "Fullscreen wheel scrolling",
            "Lines per mouse-wheel event in fullscreen mode; 'auto' speeds up fast wheel spins where the terminal does not",
            config.fullscreen_wheel_scroll_lines.map_or("auto".into(), |lines| lines.to_string()),
            &wheel_values,
        ),
        submenu_item("theme", "Theme", "Color theme for the interface", config.current_theme.clone(), submenus.theme),
    ]);
    items
}

/// `auto`, then 1, 2, 3, 5, 10, and a custom configured value, ascending.
fn wheel_scroll_values(configured: Option<u64>) -> Vec<String> {
    let mut lines: Vec<u64> = vec![1, 2, 3, 5, 10];
    lines.extend(configured);
    lines.sort_unstable();
    lines.dedup();
    std::iter::once("auto".to_string()).chain(lines.into_iter().map(|lines| lines.to_string())).collect()
}

/// Calls the callback of a changed setting with the parsed value.
pub(super) fn dispatch_change(callbacks: &mut SettingsCallbacks, id: &str, value: &str) {
    let flag = value == "true";
    let number = value.parse::<u64>().ok();
    match id {
        "autocompact" => (callbacks.on_auto_compact_change)(flag),
        "show-images" => (callbacks.on_show_images_change)(flag),
        "image-width-cells" => number.into_iter().for_each(&mut callbacks.on_image_width_cells_change),
        "auto-resize-images" => (callbacks.on_auto_resize_images_change)(flag),
        "block-images" => (callbacks.on_block_images_change)(flag),
        "skill-commands" => (callbacks.on_enable_skill_commands_change)(flag),
        "steering-mode" => parse_enum(value).into_iter().for_each(&mut callbacks.on_steering_mode_change),
        "follow-up-mode" => parse_enum(value).into_iter().for_each(&mut callbacks.on_follow_up_mode_change),
        "transport" => parse_enum(value).into_iter().for_each(&mut callbacks.on_transport_change),
        "http-idle-timeout" => {
            let choice = HTTP_IDLE_TIMEOUT_CHOICES.iter().find(|(label, _)| *label == value);
            choice.into_iter().for_each(|(_, ms)| (callbacks.on_http_idle_timeout_ms_change)(*ms));
        }
        "cache-warming-mode" => parse_enum(value).into_iter().for_each(&mut callbacks.on_cache_warming_mode_change),
        "hide-thinking" => (callbacks.on_hide_thinking_block_change)(flag),
        "mermaid-rendering" => parse_enum(value).into_iter().for_each(&mut callbacks.on_mermaid_rendering_mode_change),
        "cache-miss-notices" => (callbacks.on_show_cache_miss_notices_change)(flag),
        "collapse-changelog" => (callbacks.on_collapse_changelog_change)(flag),
        "quiet-startup" => {
            let quiet = match value {
                "header" => QuietStartup::Header,
                "true" => QuietStartup::On,
                _ => QuietStartup::Off,
            };
            (callbacks.on_quiet_startup_change)(quiet);
        }
        "install-telemetry" => (callbacks.on_enable_install_telemetry_change)(flag),
        "default-project-trust" => {
            let trust = DefaultProjectTrust::ALL.into_iter().find(|trust| trust.label() == value);
            trust.into_iter().for_each(&mut callbacks.on_default_project_trust_change);
        }
        "double-escape-action" => parse_enum(value).into_iter().for_each(&mut callbacks.on_double_escape_action_change),
        "tree-filter-mode" => parse_enum(value).into_iter().for_each(&mut callbacks.on_tree_filter_mode_change),
        "show-hardware-cursor" => (callbacks.on_show_hardware_cursor_change)(flag),
        "editor-padding" => number.into_iter().for_each(&mut callbacks.on_editor_padding_x_change),
        "output-padding" => (callbacks.on_output_pad_change)(if value == "0" { 0 } else { 1 }),
        "autocomplete-max-visible" => number.into_iter().for_each(&mut callbacks.on_autocomplete_max_visible_change),
        "clear-on-shrink" => (callbacks.on_clear_on_shrink_change)(flag),
        "terminal-progress" => (callbacks.on_show_terminal_progress_change)(flag),
        "tui-mode" => parse_enum(value).into_iter().for_each(&mut callbacks.on_tui_mode_change),
        "fullscreen-exit-output" => {
            parse_enum(value).into_iter().for_each(&mut callbacks.on_fullscreen_exit_output_change)
        }
        "fullscreen-scrollbar" => parse_enum(value).into_iter().for_each(&mut callbacks.on_fullscreen_scrollbar_change),
        "fullscreen-copy-on-select" => (callbacks.on_fullscreen_copy_on_select_change)(flag),
        "fullscreen-wheel-scroll-lines" => {
            if value == "auto" {
                (callbacks.on_fullscreen_wheel_scroll_lines_change)(None);
            } else if let Some(lines) = number {
                (callbacks.on_fullscreen_wheel_scroll_lines_change)(Some(lines));
            }
        }
        "theme" => (callbacks.on_theme_change)(value),
        _ => {}
    }
}
