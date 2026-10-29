//! Theme loading and the process-wide active theme (upstream `theme.ts` loading and global instance).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};

use pi_tui::colors::{ColorValue, TerminalColorMode};
use pi_tui::terminal_colors::TerminalColors;
use pi_tui::terminal_image::{get_capabilities, get_terminal_color_mode};

use super::ThemeError;
use super::schema::{ThemeJson, invalid_theme_name_error, parse_theme_json_content, theme_json_from_value};
use super::system::{SYSTEM_THEME_NAME, SystemThemeInput, generate_system_theme_colors};
use super::terminal::{get_terminal_theme, terminal_colors, terminal_colors_pending};
use super::theme::{Theme, ThemeOptions};
use super::tokens::TokenMap;
use super::watcher::{ThemeWatcher, start_theme_watcher};

/// Names of the themes shipped as JSON (the `system` theme is generated).
pub const BUILTIN_THEME_NAMES: [&str; 6] = ["dark", "light", "ghostty", "darcula-islands", "nord", "catppuccin"];

static BUILTIN_THEMES: LazyLock<HashMap<&'static str, ThemeJson>> = LazyLock::new(|| {
    let parse = |text: &str| theme_json_from_value(&serde_json::from_str(text).unwrap_or_default());
    HashMap::from([
        ("dark", parse(include_str!("builtin/dark.json"))),
        ("light", parse(include_str!("builtin/light.json"))),
        ("ghostty", parse(include_str!("builtin/ghostty.json"))),
        ("darcula-islands", parse(include_str!("builtin/darcula-islands.json"))),
        ("nord", parse(include_str!("builtin/nord.json"))),
        ("catppuccin", parse(include_str!("builtin/catppuccin.json"))),
    ])
});

/// The source text of a built-in theme file.
pub fn builtin_theme_source(name: &str) -> Option<&'static str> {
    match name {
        "dark" => Some(include_str!("builtin/dark.json")),
        "light" => Some(include_str!("builtin/light.json")),
        "ghostty" => Some(include_str!("builtin/ghostty.json")),
        "darcula-islands" => Some(include_str!("builtin/darcula-islands.json")),
        "nord" => Some(include_str!("builtin/nord.json")),
        "catppuccin" => Some(include_str!("builtin/catppuccin.json")),
        _ => None,
    }
}

/// A theme name and the file it comes from (`None` for built-in and in-memory themes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeInfo {
    pub name: String,
    pub path: Option<PathBuf>,
}

pub(crate) type ChangeCallback = Arc<dyn Fn() + Send + Sync>;

pub(crate) struct Registry {
    pub current: Option<Arc<Theme>>,
    pub current_name: Option<String>,
    pub registered: Vec<Arc<Theme>>,
    pub custom_dir: Option<PathBuf>,
    pub on_change: Option<ChangeCallback>,
    pub watcher: Option<ThemeWatcher>,
}

static REGISTRY: LazyLock<RwLock<Registry>> = LazyLock::new(|| {
    RwLock::new(Registry {
        current: None,
        current_name: None,
        registered: Vec::new(),
        custom_dir: None,
        on_change: None,
        watcher: None,
    })
});

pub(crate) fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    f(&mut REGISTRY.write().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

fn read_registry<T>(f: impl FnOnce(&Registry) -> T) -> T {
    f(&REGISTRY.read().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// The color mode the terminal capabilities allow.
pub fn default_color_mode() -> TerminalColorMode {
    get_terminal_color_mode(&get_capabilities())
}

/// Override the user themes directory (default `<agent-dir>/themes`); `None` restores the default.
pub fn set_custom_themes_dir(dir: Option<PathBuf>) {
    with_registry(|registry| registry.custom_dir = dir);
}

/// The user themes directory: `<agent-dir>/themes`.
pub fn custom_themes_dir() -> PathBuf {
    read_registry(|registry| registry.custom_dir.clone()).unwrap_or_else(|| crate::config::agent_dir().join("themes"))
}

/// Resolve a value through `vars`, following chains; fails on missing or circular references.
pub fn resolve_var_refs(value: &ColorValue, vars: &HashMap<String, ColorValue>) -> Result<ColorValue, ThemeError> {
    let mut visited: Vec<&str> = Vec::new();
    let mut current = value;
    loop {
        let ColorValue::Text(text) = current else { return Ok(current.clone()) };
        let lower = text.to_ascii_lowercase();
        if text.is_empty() || text.starts_with('#') || lower.starts_with("oklch(") || lower.starts_with("okhsl(") {
            return Ok(current.clone());
        }
        if visited.contains(&text.as_str()) {
            return Err(ThemeError(format!("Circular variable reference detected: {text}")));
        }
        let Some(next) = vars.get(text) else {
            return Err(ThemeError(format!("Variable reference not found: {text}")));
        };
        visited.push(text);
        current = next;
    }
}

/// Resolve every color value of a document through its variables.
pub fn resolve_theme_colors(json: &ThemeJson) -> Result<TokenMap<Option<ColorValue>>, ThemeError> {
    let mut resolved = TokenMap::from_fn(|_| None);
    for (token, value) in json.colors.iter() {
        if let Some(value) = value {
            resolved.set(token, Some(resolve_var_refs(value, &json.vars)?));
        }
    }
    Ok(resolved)
}

/// Build a theme from a document.
pub fn create_theme(
    json: &ThemeJson,
    mode: Option<TerminalColorMode>,
    source_path: Option<PathBuf>,
) -> Result<Theme, ThemeError> {
    let options = ThemeOptions {
        name: Some(json.name.clone()),
        source_path,
        appearance: json.appearance,
        ..ThemeOptions::default()
    };
    Theme::new(resolve_theme_colors(json)?, mode.unwrap_or_else(default_color_mode), options)
}

/// The system input for the current terminal colors.
fn system_input(colors: &TerminalColors) -> SystemThemeInput {
    SystemThemeInput {
        foreground: colors.foreground,
        background: colors.background,
        palette: colors.palette.clone(),
        saturation: Some(if terminal_colors_pending() { 0.0 } else { 1.0 }),
        appearance_hint: Some(get_terminal_theme()),
    }
}

/// Generate the system theme from the terminal's reported colors (grayscale while they are pending).
pub fn create_system_theme(mode: Option<TerminalColorMode>) -> Theme {
    let generated = generate_system_theme_colors(&system_input(&terminal_colors()));
    let options = ThemeOptions {
        name: Some(SYSTEM_THEME_NAME.to_string()),
        appearance: generated.appearance,
        dim: generated.dim,
        ..ThemeOptions::default()
    };
    let values = TokenMap(generated.colors.0.into_iter().map(Some).collect());
    let mode = mode.unwrap_or_else(default_color_mode);
    Theme::new(values, mode, options.clone()).unwrap_or_else(|_| {
        let defaults = TokenMap::from_fn(|_| Some(ColorValue::Text(String::new())));
        Theme::new(defaults, mode, options).expect("terminal default colors always parse")
    })
}

/// Read, validate, and build a theme file.
pub fn load_theme_from_path(path: &Path, mode: Option<TerminalColorMode>) -> Result<Theme, ThemeError> {
    let content = std::fs::read_to_string(path).map_err(|error| ThemeError(format!("{error}: {}", path.display())))?;
    let json = parse_theme_json_content(&path.display().to_string(), &content)?;
    create_theme(&json, mode, Some(path.to_path_buf()))
}

fn registered(name: &str) -> Option<Arc<Theme>> {
    read_registry(|registry| registry.registered.iter().find(|theme| theme.name() == Some(name)).cloned())
}

/// The document of a theme: built-in, a registered theme's file, or `<custom-dir>/<name>.json`.
pub fn load_theme_json(name: &str) -> Result<ThemeJson, ThemeError> {
    if let Some(json) = BUILTIN_THEMES.get(name) {
        return Ok(json.clone());
    }
    if let Some(theme) = registered(name) {
        let Some(path) = theme.source_path() else {
            return Err(ThemeError(format!("Theme \"{name}\" does not have a source path for export")));
        };
        let content = std::fs::read_to_string(path).map_err(|error| ThemeError(error.to_string()))?;
        return parse_theme_json_content(&path.display().to_string(), &content);
    }
    let path = custom_themes_dir().join(format!("{name}.json"));
    if !path.exists() {
        return Err(ThemeError(format!("Theme not found: {name}")));
    }
    let content = std::fs::read_to_string(&path).map_err(|error| ThemeError(error.to_string()))?;
    parse_theme_json_content(name, &content)
}

pub(crate) fn load_theme(name: &str, mode: Option<TerminalColorMode>) -> Result<Arc<Theme>, ThemeError> {
    // The system theme name is reserved: it takes precedence over custom themes of the same name.
    if name == SYSTEM_THEME_NAME {
        return Ok(Arc::new(create_system_theme(mode)));
    }
    if let Some(theme) = registered(name) {
        return Ok(theme);
    }
    Ok(Arc::new(create_theme(&load_theme_json(name)?, mode, None)?))
}

/// A theme by name, or `None` when it does not exist or is invalid (upstream `getThemeByName`).
pub fn get_theme_by_name(name: &str) -> Option<Arc<Theme>> {
    load_theme(name, None).ok()
}

/// Extension API name for [`get_theme_by_name`].
pub fn get_theme(name: &str) -> Option<Arc<Theme>> {
    get_theme_by_name(name)
}

fn custom_theme_infos() -> Vec<ThemeInfo> {
    let Ok(entries) = std::fs::read_dir(custom_themes_dir()) else { return Vec::new() };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            // Invalid themes are skipped here; the resource loader reports them.
            let theme = load_theme_from_path(&path, None).ok()?;
            Some(ThemeInfo { name: theme.name()?.to_string(), path: Some(path) })
        })
        .collect()
}

/// All selectable themes, `system` first and the rest by name (upstream `getAvailableThemesWithPaths`).
pub fn get_available_themes_with_paths() -> Vec<ThemeInfo> {
    let mut result: Vec<ThemeInfo> = Vec::new();
    let mut add = |info: ThemeInfo| {
        if !result.iter().any(|existing| existing.name == info.name) {
            result.push(info);
        }
    };
    add(ThemeInfo { name: SYSTEM_THEME_NAME.to_string(), path: None });
    for name in BUILTIN_THEME_NAMES {
        add(ThemeInfo { name: name.to_string(), path: None });
    }
    for info in custom_theme_infos() {
        add(info);
    }
    for theme in read_registry(|registry| registry.registered.clone()) {
        if let Some(name) = theme.name() {
            add(ThemeInfo { name: name.to_string(), path: theme.source_path().map(Path::to_path_buf) });
        }
    }
    result.sort_by(|a, b| match (a.name == SYSTEM_THEME_NAME, b.name == SYSTEM_THEME_NAME) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => locale_compare(&a.name, &b.name),
    });
    result
}

/// Approximates JavaScript `localeCompare`: case-insensitive first, then lowercase before uppercase.
fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

pub fn get_available_themes() -> Vec<String> {
    get_available_themes_with_paths().into_iter().map(|info| info.name).collect()
}

/// Extension API name for [`get_available_themes_with_paths`].
pub fn get_all_themes() -> Vec<ThemeInfo> {
    get_available_themes_with_paths()
}

/// Replace the themes loaded from resources and extensions. Fails on names containing `/`.
pub fn set_registered_themes(themes: Vec<Arc<Theme>>) -> Result<(), ThemeError> {
    if let Some(name) = themes.iter().filter_map(|theme| theme.name()).find(|name| name.contains('/')) {
        return Err(invalid_theme_name_error(name));
    }
    let themes = themes.into_iter().filter(|theme| theme.name().is_some()).collect();
    with_registry(|registry| registry.registered = themes);
    Ok(())
}

/// Replace one registered theme (hot reload) or add it.
pub(crate) fn upsert_registered(theme: Arc<Theme>) {
    with_registry(|registry| {
        let name = theme.name().map(String::from);
        registry.registered.retain(|existing| existing.name().map(String::from) != name);
        registry.registered.push(theme);
    });
}

/// The active theme. Before [`init_theme`] runs, this is the `system` theme.
pub fn theme() -> Arc<Theme> {
    if let Some(theme) = read_registry(|registry| registry.current.clone()) {
        return theme;
    }
    let theme = Arc::new(create_system_theme(None));
    with_registry(|registry| registry.current.get_or_insert(theme).clone())
}

/// The name the active theme was selected by (`<in-memory>` for [`set_theme_instance`]).
pub fn current_theme_name() -> Option<String> {
    read_registry(|registry| registry.current_name.clone())
}

pub(crate) fn set_global(theme: Arc<Theme>) {
    with_registry(|registry| registry.current = Some(theme));
}

fn notify_changed() {
    if let Some(callback) = read_registry(|registry| registry.on_change.clone()) {
        callback();
    }
}

/// Activate a theme, falling back to `system` silently when it cannot be loaded.
pub fn init_theme(name: Option<&str>, enable_watcher: bool) {
    let name = name.unwrap_or(SYSTEM_THEME_NAME);
    match load_theme(name, None) {
        Ok(theme) => {
            with_registry(|registry| registry.current_name = Some(name.to_string()));
            set_global(theme);
            if enable_watcher {
                restart_watcher(name);
            }
        }
        Err(_) => fall_back_to_system(),
    }
}

fn fall_back_to_system() {
    with_registry(|registry| registry.current_name = Some(SYSTEM_THEME_NAME.to_string()));
    set_global(Arc::new(create_system_theme(None)));
}

/// Activate a theme by name. On failure the `system` theme is active and the error is returned.
pub fn set_theme(name: &str, enable_watcher: bool) -> Result<(), ThemeError> {
    with_registry(|registry| registry.current_name = Some(name.to_string()));
    match load_theme(name, None) {
        Ok(theme) => {
            set_global(theme);
            if enable_watcher {
                restart_watcher(name);
            }
            notify_changed();
            Ok(())
        }
        Err(error) => {
            fall_back_to_system();
            Err(error)
        }
    }
}

/// Activate an in-memory theme. File watching stops because there is no file to watch.
pub fn set_theme_instance(theme: Arc<Theme>) {
    set_global(theme);
    with_registry(|registry| registry.current_name = Some("<in-memory>".to_string()));
    stop_theme_watcher();
    notify_changed();
}

/// Called after the active theme changes (selection, hot reload). Runs on the thread that changed it.
pub fn on_theme_change(callback: impl Fn() + Send + Sync + 'static) {
    with_registry(|registry| registry.on_change = Some(Arc::new(callback)));
}

fn restart_watcher(name: &str) {
    stop_theme_watcher();
    if name == SYSTEM_THEME_NAME || BUILTIN_THEME_NAMES.contains(&name) {
        return;
    }
    let path = custom_themes_dir().join(format!("{name}.json"));
    if !path.exists() {
        return;
    }
    if let Some(watcher) = start_theme_watcher(name.to_string(), path, notify_changed) {
        with_registry(|registry| registry.watcher = Some(watcher));
    }
}

/// Stop watching the active theme's file.
pub fn stop_theme_watcher() {
    let watcher = with_registry(|registry| registry.watcher.take());
    drop(watcher);
}
