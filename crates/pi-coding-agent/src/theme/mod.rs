//! Themes for interactive mode and HTML export, ported from upstream `modes/interactive/theme/*`.
//! Built-in `dark`, `light`, and the generated `system` theme; theme JSON files with validation;
//! light/dark selection; hot reload; and the theme structs the TUI components take.

mod components;
mod controller;
mod export;
mod registry;
mod resources;
mod schema;
mod system;
mod system_recipe;
mod terminal;
#[allow(clippy::module_inception)]
mod theme;
mod tokens;
mod tui_themes;
mod watcher;

use std::fmt;

pub use components::{
    CLI_HIGHLIGHT_CLASSES, HighlightStyle, cli_highlight_theme, get_editor_theme, get_language_from_path,
    get_markdown_theme, get_select_list_theme, get_settings_list_theme, highlight_code,
};
pub use controller::{
    InteractiveThemeController, TERMINAL_QUERY_TIMEOUT, ThemeControllerOptions, ThemeHost, ThemeSelection,
};
pub use export::{ThemeExportColors, export_theme, get_resolved_theme_colors, get_theme_export_colors, is_light_theme};
pub use registry::{
    BUILTIN_THEME_NAMES, ThemeInfo, builtin_theme_source, create_system_theme, create_theme, current_theme_name,
    custom_themes_dir, default_color_mode, get_all_themes, get_available_themes, get_available_themes_with_paths,
    get_theme, get_theme_by_name, init_theme, load_theme_from_path, load_theme_json, on_theme_change,
    resolve_theme_colors, resolve_var_refs, set_custom_themes_dir, set_registered_themes, set_theme,
    set_theme_instance, stop_theme_watcher, theme,
};
pub use resources::{LoadedThemes, load_resource_themes, load_theme_files};
pub use schema::{
    THEME_SCHEMA_URL, ThemeExportJson, ThemeJson, parse_theme_json_content, theme_json_schema, validate_theme_json,
};
pub use system::{
    SYSTEM_THEME_NAME, SystemThemeColors, SystemThemeInput, generate_system_theme_colors, relative_luminance,
    terminal_appearance, wcag_contrast,
};
pub use terminal::{
    AutoThemeSetting, TerminalTheme, ThemeAppearance, detect_color_fg_bg_theme, detect_terminal_theme,
    get_terminal_theme, mark_terminal_colors_pending, parse_auto_theme_setting, resolve_theme_setting,
    set_terminal_color_scheme, set_terminal_colors, set_theme_environment, terminal_colors,
};
pub use theme::{StyleBg, StyleFg, Theme, ThemeOptions, ThemeStyle, with_theme_token_fallbacks};
pub use tokens::{THEME_TOKENS, TOKEN_COUNT, ThemeBg, ThemeColor, ThemeToken, TokenDescriptor, TokenMap, TokenSlot};
pub use tui_themes::{
    EditorTheme, HighlightCodeFn, MarkdownTheme, SelectListTheme, SelectedStyleFn, SettingsListTheme, StyleFn,
};

/// A theme that could not be loaded, validated, or applied. The message matches upstream's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeError(pub String);

impl fmt::Display for ThemeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ThemeError {}

#[cfg(test)]
pub(crate) mod tests;
