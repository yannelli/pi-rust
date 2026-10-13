//! The terminal's reported colors and appearance, and the `light/dark` theme setting format.

use std::fmt;
use std::sync::{Arc, LazyLock, RwLock};

use pi_tui::env::{EnvLookup, ProcessEnv};
use pi_tui::terminal_colors::{TerminalColorScheme, TerminalColors};
use serde::{Deserialize, Serialize};

use super::system::terminal_appearance;

/// Whether a terminal (or the background a theme is designed for) is dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalTheme {
    Dark,
    Light,
}

/// The background a theme is designed for.
pub type ThemeAppearance = TerminalTheme;

impl TerminalTheme {
    pub fn as_str(self) -> &'static str {
        match self {
            TerminalTheme::Dark => "dark",
            TerminalTheme::Light => "light",
        }
    }
}

impl fmt::Display for TerminalTheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<TerminalColorScheme> for TerminalTheme {
    fn from(scheme: TerminalColorScheme) -> Self {
        match scheme {
            TerminalColorScheme::Dark => TerminalTheme::Dark,
            TerminalColorScheme::Light => TerminalTheme::Light,
        }
    }
}

type SharedEnv = Arc<dyn EnvLookup + Send + Sync>;

struct TerminalState {
    // Replaced (never mutated) on update, so themes cache resolved colors by identity.
    colors: Arc<TerminalColors>,
    pending: bool,
    scheme: Option<TerminalTheme>,
    env: Option<SharedEnv>,
}

static STATE: LazyLock<RwLock<TerminalState>> = LazyLock::new(|| {
    RwLock::new(TerminalState { colors: Arc::new(TerminalColors::default()), pending: false, scheme: None, env: None })
});

fn read<T>(f: impl FnOnce(&TerminalState) -> T) -> T {
    f(&STATE.read().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

fn write<T>(f: impl FnOnce(&mut TerminalState) -> T) -> T {
    f(&mut STATE.write().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// Record the terminal's reported colors and end the pending state. Themes use the default colors for
/// tokens set to `""`; the system theme is generated from all of them.
pub fn set_terminal_colors(colors: TerminalColors) {
    write(|state| {
        state.colors = Arc::new(colors);
        state.pending = false;
    });
}

/// The terminal's last reported colors.
pub fn terminal_colors() -> Arc<TerminalColors> {
    read(|state| state.colors.clone())
}

/// Record the terminal's light/dark report, the fallback for terminals that do not report their background.
pub fn set_terminal_color_scheme(scheme: Option<TerminalTheme>) {
    write(|state| state.scheme = scheme);
}

/// Render the system theme in grayscale until [`set_terminal_colors`] reports the terminal's colors.
pub fn mark_terminal_colors_pending() {
    write(|state| state.pending = true);
}

pub(crate) fn terminal_colors_pending() -> bool {
    read(|state| state.pending)
}

/// Replace the environment read for `COLORFGBG` (tests and embedders); `None` reads the process environment.
pub fn set_theme_environment(env: Option<Arc<dyn EnvLookup + Send + Sync>>) {
    write(|state| state.env = env);
}

/// Dark or light from `COLORFGBG` (`fg;bg` or rxvt's `fg;xpm;bg`), classified by palette index like Vim:
/// 0-6 and 8 are dark, 7 and 9-15 are light. `None` without a usable background index.
pub fn detect_color_fg_bg_theme(env: &dyn EnvLookup) -> Option<TerminalTheme> {
    let value = env.var("COLORFGBG")?;
    let bg = value.rsplit(';').next()?.trim();
    if bg.is_empty() || bg.len() > 2 || !bg.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index: u8 = bg.parse().ok()?;
    if index > 15 {
        return None;
    }
    Some(if index <= 6 || index == 8 { TerminalTheme::Dark } else { TerminalTheme::Light })
}

/// Whether the terminal is dark or light: the reported background decides; without one, the terminal's
/// light/dark report, then `COLORFGBG`, then dark.
pub fn detect_terminal_theme(
    colors: &TerminalColors,
    reported: Option<TerminalTheme>,
    env: &dyn EnvLookup,
) -> TerminalTheme {
    if let Some(background) = colors.background {
        return terminal_appearance(background, colors.foreground);
    }
    reported.or_else(|| detect_color_fg_bg_theme(env)).unwrap_or(TerminalTheme::Dark)
}

/// Whether the terminal is dark or light, from everything it reported so far.
pub fn get_terminal_theme() -> TerminalTheme {
    let (colors, scheme, env) = read(|state| (state.colors.clone(), state.scheme, state.env.clone()));
    match env {
        Some(env) => detect_terminal_theme(&colors, scheme, env.as_ref()),
        None => detect_terminal_theme(&colors, scheme, &ProcessEnv),
    }
}

/// The two themes of an automatic `light/dark` setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoThemeSetting {
    pub light_theme: String,
    pub dark_theme: String,
}

/// Parse a `light/dark` setting; `None` unless it has exactly one `/` and two non-empty names.
pub fn parse_auto_theme_setting(setting: Option<&str>) -> Option<AutoThemeSetting> {
    let (light, dark) = setting?.split_once('/')?;
    if dark.contains('/') {
        return None;
    }
    let (light, dark) = (light.trim(), dark.trim());
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some(AutoThemeSetting { light_theme: light.to_string(), dark_theme: dark.to_string() })
}

/// The theme name a setting selects for a terminal appearance.
pub fn resolve_theme_setting(setting: Option<&str>, terminal: TerminalTheme) -> Option<String> {
    if let Some(auto) = parse_auto_theme_setting(setting) {
        return Some(match terminal {
            TerminalTheme::Light => auto.light_theme,
            TerminalTheme::Dark => auto.dark_theme,
        });
    }
    setting.filter(|setting| !setting.contains('/')).map(String::from)
}
