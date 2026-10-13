//! The [`Theme`] type: precomputed escape sequences per token plus the concrete colors behind them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi_ai::ThinkingLevel;
use pi_tui::colors::{
    Color, ColorMixSpace, ColorValue, TerminalColorMode, TextAttributes, background_ansi, color_to_oklch,
    foreground_ansi, mix_colors, parse_color, parse_color_value, style_text_with_ansi,
};
use pi_tui::terminal_colors::{RgbColor, TerminalColors};

use super::ThemeError;
use super::terminal::{ThemeAppearance, get_terminal_theme, terminal_colors};
use super::tokens::{ThemeBg, ThemeColor, ThemeToken, TokenMap, TokenSlot};
use crate::resources::SourceInfo;

/// A foreground for [`Theme::style`]: a theme token or a concrete color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StyleFg {
    Token(ThemeColor),
    Color(Color),
}

/// A background for [`Theme::style`]: a theme token or a concrete color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StyleBg {
    Token(ThemeBg),
    Color(Color),
}

/// Colors plus text attributes (upstream `ThemeStyle`). Tokens are only accepted in their own slot,
/// because `""` means the default foreground or background depending on the slot.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ThemeStyle {
    pub fg: Option<StyleFg>,
    pub bg: Option<StyleBg>,
    pub attributes: TextAttributes,
}

/// Construction options for [`Theme::new`].
#[derive(Debug, Clone, Default)]
pub struct ThemeOptions {
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    pub source_info: Option<SourceInfo>,
    pub appearance: Option<ThemeAppearance>,
    /// Foreground tokens to render faint (SGR 2).
    pub dim: Vec<ThemeColor>,
}

/// Fill optional tokens from their fallbacks (`scrollbarTrack` from `muted`, and so on).
pub fn with_theme_token_fallbacks<V: Clone>(values: TokenMap<Option<V>>) -> Result<TokenMap<V>, ThemeError> {
    let mut resolved = Vec::with_capacity(values.0.len());
    for (token, value) in values.iter() {
        let value = match (value, token.fallback()) {
            (Some(value), _) => value.clone(),
            (None, Some(fallback)) => values[fallback].clone().ok_or_else(|| {
                ThemeError(format!("Theme token {} has unresolved fallback {}", token.name(), fallback.name()))
            })?,
            (None, None) => return Err(ThemeError(format!("Theme token {} has no value", token.name()))),
        };
        resolved.push(value);
    }
    Ok(TokenMap(resolved))
}

/// Assumed terminal default colors when the terminal does not report them.
fn guessed_default_colors(appearance: ThemeAppearance) -> (Color, Color) {
    let parse = |hex: &str| parse_color(hex).unwrap_or(Color::Indexed(0));
    match appearance {
        ThemeAppearance::Dark => (parse("#e5e5e7"), parse("#000000")),
        ThemeAppearance::Light => (parse("#000000"), parse("#ffffff")),
    }
}

fn average_lightness(colors: &[Color]) -> Option<f64> {
    // Palette colors 0-15 follow the user's terminal palette, so they say nothing about the theme.
    let fixed: Vec<&Color> =
        colors.iter().filter(|color| !matches!(color, Color::Indexed(index) if *index < 16)).collect();
    if fixed.is_empty() {
        return None;
    }
    Some(fixed.iter().map(|color| color_to_oklch(color).l).sum::<f64>() / fixed.len() as f64)
}

/// Detect the background a theme is designed for from the lightness of its own colors.
fn detect_appearance(foregrounds: &[Color], backgrounds: &[Color]) -> Option<ThemeAppearance> {
    let dark_if = |dark: bool| if dark { ThemeAppearance::Dark } else { ThemeAppearance::Light };
    match (average_lightness(foregrounds), average_lightness(backgrounds)) {
        (Some(fg), Some(bg)) => Some(dark_if(bg < fg)),
        (None, Some(bg)) => Some(dark_if(bg < 0.5)),
        (Some(fg), None) => Some(dark_if(fg > 0.5)),
        (None, None) => None,
    }
}

fn rgb(color: RgbColor) -> Color {
    Color::Rgb { r: f64::from(color.r), g: f64::from(color.g), b: f64::from(color.b) }
}

type ResolvedCache = Option<(Arc<TerminalColors>, Arc<TokenMap<Color>>)>;

/// A loaded theme. Cheap to share as `Arc<Theme>`; [`Theme::fg`] and [`Theme::bg`] are a lookup and a concat.
#[derive(Debug)]
pub struct Theme {
    name: Option<String>,
    source_path: Option<PathBuf>,
    source_info: Option<SourceInfo>,
    mode: TerminalColorMode,
    fg_ansi: Vec<String>,
    bg_ansi: Vec<String>,
    concrete: TokenMap<Option<Color>>,
    default_background: Vec<ThemeToken>,
    dim: Vec<ThemeColor>,
    own_appearance: Option<ThemeAppearance>,
    resolved: Mutex<ResolvedCache>,
}

impl Theme {
    /// Build a theme from resolved color values (`""` is the terminal default; numbers are palette indices).
    /// Optional tokens left out take their fallback token's value.
    pub fn new(
        values: TokenMap<Option<ColorValue>>,
        mode: TerminalColorMode,
        options: ThemeOptions,
    ) -> Result<Theme, ThemeError> {
        let values = with_theme_token_fallbacks(values)?;
        let mut concrete = TokenMap::from_fn(|_| None);
        let mut default_background = Vec::new();
        let (mut foregrounds, mut backgrounds) = (Vec::new(), Vec::new());
        let mut ansi = TokenMap::from_fn(|_| String::new());
        for (token, value) in values.iter() {
            let background = token.slot() == TokenSlot::Background;
            if matches!(value, ColorValue::Text(text) if text.is_empty()) {
                if background {
                    default_background.push(token);
                    ansi.set(token, "\x1b[49m".to_string());
                } else {
                    ansi.set(token, "\x1b[39m".to_string());
                }
                continue;
            }
            let color = parse_color_value(value).map_err(|error| ThemeError(error.0))?;
            concrete.set(token, Some(color));
            if background {
                backgrounds.push(color);
                ansi.set(token, background_ansi(&color, mode));
            } else {
                foregrounds.push(color);
                ansi.set(token, foreground_ansi(&color, mode));
            }
        }
        Ok(Theme {
            name: options.name,
            source_path: options.source_path,
            source_info: options.source_info,
            mode,
            fg_ansi: ThemeColor::ALL.iter().map(|color| ansi[color.token()].clone()).collect(),
            bg_ansi: ThemeBg::ALL.iter().map(|bg| ansi[bg.token()].clone()).collect(),
            concrete,
            default_background,
            dim: options.dim,
            own_appearance: options.appearance.or_else(|| detect_appearance(&foregrounds, &backgrounds)),
            resolved: Mutex::new(None),
        })
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn source_path(&self) -> Option<&Path> {
        self.source_path.as_deref()
    }

    pub fn source_info(&self) -> Option<&SourceInfo> {
        self.source_info.as_ref()
    }

    pub fn set_source_info(&mut self, source_info: Option<SourceInfo>) {
        self.source_info = source_info;
    }

    /// The background the theme is designed for: declared, detected from its colors, or the terminal's.
    pub fn appearance(&self) -> ThemeAppearance {
        self.own_appearance.unwrap_or_else(get_terminal_theme)
    }

    fn is_dim(&self, color: ThemeColor) -> bool {
        self.dim.contains(&color)
    }

    /// Concrete colors for all tokens. Terminal-default tokens use the reported default colors (or a guess
    /// from the appearance); faint tokens are approximated by mixing their color toward the background.
    pub fn colors(&self) -> Arc<TokenMap<Color>> {
        let terminal = terminal_colors();
        let mut cache = self.resolved.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((cached_terminal, colors)) = cache.as_ref()
            && Arc::ptr_eq(cached_terminal, &terminal)
        {
            return colors.clone();
        }
        let (guess_fg, guess_bg) = guessed_default_colors(self.appearance());
        let foreground = terminal.foreground.map(rgb).unwrap_or(guess_fg);
        let background = terminal.background.map(rgb).unwrap_or(guess_bg);
        let mut colors = TokenMap::from_fn(|token| self.concrete[token].unwrap_or(foreground));
        for &token in &self.default_background {
            colors.set(token, background);
        }
        for &color in &self.dim {
            let mixed = mix_colors(colors.get(color), &background, 0.4, ColorMixSpace::Oklch);
            if let Ok(mixed) = mixed {
                colors.set(color, mixed);
            }
        }
        let colors = Arc::new(colors);
        *cache = Some((terminal, colors.clone()));
        colors
    }

    /// The concrete color of one token (see [`Theme::colors`]).
    pub fn color(&self, token: impl Into<ThemeToken>) -> Color {
        *self.colors().get(token)
    }

    pub fn style(&self, text: &str, style: &ThemeStyle) -> String {
        let mut attributes = style.attributes;
        let fg = style.fg.map(|fg| match fg {
            StyleFg::Token(color) => {
                attributes.dim |= self.is_dim(color);
                self.fg_ansi[color as usize].clone()
            }
            StyleFg::Color(color) => foreground_ansi(&color, self.mode),
        });
        let bg = style.bg.map(|bg| match bg {
            StyleBg::Token(token) => self.bg_ansi[token as usize].clone(),
            StyleBg::Color(color) => background_ansi(&color, self.mode),
        });
        style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), &attributes)
    }

    /// Like [`Theme::style`], with tokens given by name (for extensions). Fails on unknown names and on
    /// tokens used in the wrong slot.
    pub fn style_named(
        &self,
        text: &str,
        fg: Option<&str>,
        bg: Option<&str>,
        attributes: TextAttributes,
    ) -> Result<String, ThemeError> {
        let fg = fg.map(|name| ThemeColor::from_name(name).map(StyleFg::Token).ok_or_else(|| unknown(name)));
        let bg = bg.map(|name| ThemeBg::from_name(name).map(StyleBg::Token).ok_or_else(|| unknown(name)));
        Ok(self.style(text, &ThemeStyle { fg: fg.transpose()?, bg: bg.transpose()?, attributes }))
    }

    pub fn fg(&self, color: ThemeColor, text: &str) -> String {
        let ansi = &self.fg_ansi[color as usize];
        if self.is_dim(color) {
            return format!("{ansi}\x1b[2m{text}\x1b[22;39m");
        }
        format!("{ansi}{text}\x1b[39m")
    }

    pub fn bg(&self, color: ThemeBg, text: &str) -> String {
        format!("{}{text}\x1b[49m", self.bg_ansi[color as usize])
    }

    /// [`Theme::fg`] with the token given by name.
    pub fn fg_named(&self, color: &str, text: &str) -> Result<String, ThemeError> {
        ThemeColor::from_name(color).map(|color| self.fg(color, text)).ok_or_else(|| unknown(color))
    }

    /// [`Theme::bg`] with the token given by name.
    pub fn bg_named(&self, color: &str, text: &str) -> Result<String, ThemeError> {
        ThemeBg::from_name(color).map(|color| self.bg(color, text)).ok_or_else(|| unknown(color))
    }

    pub fn bold(&self, text: &str) -> String {
        chalk(text, "\x1b[1m", "\x1b[22m")
    }

    pub fn italic(&self, text: &str) -> String {
        chalk(text, "\x1b[3m", "\x1b[23m")
    }

    pub fn underline(&self, text: &str) -> String {
        chalk(text, "\x1b[4m", "\x1b[24m")
    }

    pub fn inverse(&self, text: &str) -> String {
        chalk(text, "\x1b[7m", "\x1b[27m")
    }

    pub fn strikethrough(&self, text: &str) -> String {
        chalk(text, "\x1b[9m", "\x1b[29m")
    }

    /// Opening sequence for a foreground token. Faint tokens include SGR 2, which `\x1b[22m` closes.
    pub fn get_fg_ansi(&self, color: ThemeColor) -> String {
        let ansi = &self.fg_ansi[color as usize];
        if self.is_dim(color) { format!("{ansi}\x1b[2m") } else { ansi.clone() }
    }

    pub fn get_bg_ansi(&self, color: ThemeBg) -> &str {
        &self.bg_ansi[color as usize]
    }

    pub fn get_color_mode(&self) -> TerminalColorMode {
        self.mode
    }

    /// The border color token for a thinking level.
    pub fn thinking_border_token(level: ThinkingLevel) -> ThemeColor {
        match level {
            ThinkingLevel::Off => ThemeColor::ThinkingOff,
            ThinkingLevel::Minimal => ThemeColor::ThinkingMinimal,
            ThinkingLevel::Low => ThemeColor::ThinkingLow,
            ThinkingLevel::Medium => ThemeColor::ThinkingMedium,
            ThinkingLevel::High => ThemeColor::ThinkingHigh,
            ThinkingLevel::Xhigh => ThemeColor::ThinkingXhigh,
            ThinkingLevel::Max => ThemeColor::ThinkingMax,
        }
    }

    pub fn get_thinking_border_color(self: &Arc<Self>, level: ThinkingLevel) -> impl Fn(&str) -> String + use<> {
        let (theme, color) = (self.clone(), Theme::thinking_border_token(level));
        move |text| theme.fg(color, text)
    }

    pub fn get_bash_mode_border_color(self: &Arc<Self>) -> impl Fn(&str) -> String + use<> {
        let theme = self.clone();
        move |text| theme.fg(ThemeColor::BashMode, text)
    }
}

fn unknown(name: &str) -> ThemeError {
    ThemeError(format!("Unknown theme color: {name}"))
}

/// Wrap `text` like chalk: re-open after inner closes, close and re-open around line breaks, skip empty text.
pub(crate) fn chalk(text: &str, open: &str, close: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let reopened;
    let mut body = text;
    if body.contains('\x1b') {
        reopened = body.replace(close, &format!("{close}{open}"));
        body = &reopened;
    }
    let mut out = String::with_capacity(body.len() + 16);
    out.push_str(open);
    let mut lines = body.split('\n').peekable();
    while let Some(line) = lines.next() {
        if lines.peek().is_none() {
            out.push_str(line);
            break;
        }
        match line.strip_suffix('\r') {
            Some(line) => out.push_str(&format!("{line}{close}\r\n{open}")),
            None => out.push_str(&format!("{line}{close}\n{open}")),
        }
    }
    out.push_str(close);
    out
}
