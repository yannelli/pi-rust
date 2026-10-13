//! The `system` theme: pi's colors derived from the terminal's own colors (upstream `system-theme.ts`).
//! Hue comes from the terminal palette, lightness from contrast rules against the background, so every
//! token reaches its contrast level on the surfaces it is drawn on. See `system_recipe` for the rules.

use pi_tui::colors::{Color, ColorValue, color_to_oklch, color_to_rgb, oklch_color};
use pi_tui::oklab::{okhsl_to_rgb, oklab_to_okhsl_lightness, rgb_to_okhsl};
use pi_tui::terminal_colors::RgbColor;

use super::system_recipe::{
    FOREGROUND_TOKENS, Family, Level, NEUTRAL, Rule, Surface, is_panel, level_curve, rules, solve_order, token_family,
    token_slot,
};
use super::terminal::TerminalTheme;
use super::tokens::{ThemeColor, ThemeToken, TokenMap};

/// The reserved name of the generated theme.
pub const SYSTEM_THEME_NAME: &str = "system";

/// Relaxation compresses levels stronger than this one toward it before weakening all levels.
fn readable_floor(appearance: TerminalTheme) -> Level {
    match appearance {
        TerminalTheme::Dark => Level::Readable,
        TerminalTheme::Light => Level::Subtle,
    }
}

/// Body text uses the terminal's foreground when it reaches this level.
const FOREGROUND_LEVEL: Level = Level::Emphasis;

/// WCAG 2 contrast ratio that body text must reach on the surfaces it is drawn on.
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// What the terminal reported, plus rendering hints.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemThemeInput {
    pub foreground: Option<RgbColor>,
    pub background: Option<RgbColor>,
    /// ANSI colors 0-15.
    pub palette: Option<Vec<RgbColor>>,
    /// Saturation multiplier from 0 (grayscale) to 1. The first frame renders grayscale until colors arrive.
    pub saturation: Option<f64>,
    /// Appearance when the terminal did not report its background (light/dark report or COLORFGBG).
    pub appearance_hint: Option<TerminalTheme>,
}

/// The generated colors: hex strings, palette indices, or `""` for the terminal default.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemThemeColors {
    pub colors: TokenMap<ColorValue>,
    /// Foreground tokens rendered faint (SGR 2), for terminals that did not report colors.
    pub dim: Vec<ThemeColor>,
    pub appearance: Option<TerminalTheme>,
}

fn oklab_lightness(color: RgbColor) -> f64 {
    color_to_oklch(&rgb(color)).l
}

fn rgb(color: RgbColor) -> Color {
    Color::Rgb { r: f64::from(color.r), g: f64::from(color.g), b: f64::from(color.b) }
}

/// WCAG 2 relative luminance.
pub fn relative_luminance(color: RgbColor) -> f64 {
    let linear = |channel: u8| {
        let value = f64::from(channel) / 255.0;
        if value <= 0.04045 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// WCAG 2 contrast ratio, 1-21.
pub fn wcag_contrast(first: RgbColor, second: RgbColor) -> f64 {
    let (a, b) = (relative_luminance(first), relative_luminance(second));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

const WHITE: RgbColor = RgbColor::new(255, 255, 255);
const BLACK: RgbColor = RgbColor::new(0, 0, 0);

/// Whether a terminal is dark or light: its foreground's direction when text can be readable that way,
/// otherwise dark when white text has more contrast on the background than black text.
pub fn terminal_appearance(background: RgbColor, foreground: Option<RgbColor>) -> TerminalTheme {
    let white_contrast = wcag_contrast(WHITE, background);
    let black_contrast = wcag_contrast(BLACK, background);
    if let Some(foreground) = foreground {
        let (foreground_l, background_l) = (oklab_lightness(foreground), oklab_lightness(background));
        if (foreground_l - background_l).abs() > 0.05 {
            let dark = foreground_l > background_l;
            let best = if dark { white_contrast } else { black_contrast };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return if dark { TerminalTheme::Dark } else { TerminalTheme::Light };
            }
        }
    }
    if white_contrast >= black_contrast { TerminalTheme::Dark } else { TerminalTheme::Light }
}

fn hex_of(color: RgbColor) -> String {
    format!("#{:02x}{:02x}{:02x}", color.r, color.g, color.b)
}

/// Saturation weight at a lightness: a Gaussian (center 0.5, sigma 0.25), 0 at black and white, 1 in the middle.
fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| (-((x - 0.5).powi(2)) / (2.0 * 0.25f64.powi(2))).exp();
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

fn saturation_curve(family: &Family, lightness: f64) -> f64 {
    let floor = if family.max > 0.0 { family.min / family.max } else { 1.0 };
    floor + (1.0 - floor) * bell_weight(lightness)
}

fn level_target(level: Level, appearance: TerminalTheme, surface_l: f64) -> Option<f64> {
    let curve = level_curve(level, appearance);
    if surface_l < curve.reachable[0] || surface_l > curve.reachable[1] {
        return None;
    }
    let terms = curve.coefficients.iter().enumerate();
    Some(terms.fold(0.0, |sum, (power, coefficient)| sum + coefficient * surface_l.powf(power as f64)))
}

/// A terminal color's OKHSL channels and its OKLCH chroma.
#[derive(Debug, Clone, Copy)]
struct SourceColor {
    h: f64,
    s: f64,
    l: f64,
    chroma: f64,
}

fn source_of(color: RgbColor) -> SourceColor {
    let okhsl = rgb_to_okhsl([f64::from(color.r), f64::from(color.g), f64::from(color.b)]);
    SourceColor { h: okhsl.h, s: okhsl.s, l: okhsl.l, chroma: color_to_oklch(&rgb(color)).c }
}

fn okhsl(h: f64, s: f64, l: f64) -> RgbColor {
    okhsl_to_rgb(h, s.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
}

/// A source color's hue at another lightness, never gaining saturation or OKLCH chroma over the source.
fn anchored(source: SourceColor, family: &Family, lightness: f64, saturation: f64) -> RgbColor {
    let anchor = saturation_curve(family, source.l);
    let falloff = if anchor > 0.0 { (saturation_curve(family, lightness) / anchor).min(1.0) } else { 1.0 };
    let color = okhsl(source.h, source.s * falloff * saturation, lightness);
    let cap = source.chroma * falloff * saturation;
    let channels = color_to_oklch(&rgb(color));
    if channels.c <= cap {
        return color;
    }
    oklch_color(channels.l.clamp(0.0, 1.0), cap.max(0.0), source.h).map(|capped| color_to_rgb(&capped)).unwrap_or(color)
}

/// Move a text color toward white or black until it reaches the WCAG minimum on every surface.
fn with_text_contrast(color: RgbColor, surfaces: &[RgbColor], lighter: bool) -> RgbColor {
    let meets = |candidate: RgbColor| {
        surfaces.iter().all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }
    let channels = rgb_to_okhsl([f64::from(color.r), f64::from(color.g), f64::from(color.b)]);
    let at = |lightness: f64| okhsl(channels.h, channels.s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }
    let (mut low, mut high) = (channels.l, extreme);
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

/// Colors for terminals that reported nothing: palette indices and default colors the terminal renders.
fn indexed_colors(saturation: f64, appearance: Option<TerminalTheme>) -> SystemThemeColors {
    let mut dim = Vec::new();
    let colors = TokenMap::from_fn(|token| {
        if is_panel(token) {
            return ColorValue::Text(String::new());
        }
        let (_, neutral) = token_family(token);
        if neutral
            && let Ok(color) = ThemeColor::try_from(token)
            && !FOREGROUND_TOKENS.contains(&color)
        {
            dim.push(color);
        }
        if !neutral && saturation > 0.0 {
            ColorValue::Number(token_slot(token) as f64)
        } else {
            ColorValue::Text(String::new())
        }
    });
    SystemThemeColors { colors, dim, appearance }
}

struct Solver {
    rules: Vec<Rule>,
    order: Vec<ThemeToken>,
    appearance: TerminalTheme,
    background: RgbColor,
    background_l: f64,
    palette: Option<Vec<SourceColor>>,
    saturation: f64,
    extreme_text: RgbColor,
}

impl Solver {
    fn lighter(&self) -> bool {
        self.appearance == TerminalTheme::Dark
    }

    fn paint(&self, token: ThemeToken, oklab_l: f64) -> RgbColor {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let (family, _) = token_family(token);
        match &self.palette {
            None => {
                let saturation = (family.min + (family.max - family.min) * bell_weight(lightness)) * self.saturation;
                okhsl(family.hue, saturation, lightness)
            }
            Some(palette) => anchored(palette[token_slot(token)], &family, lightness, self.saturation),
        }
    }

    /// The lightness a level needs on a surface, relaxed by `t` (0-1 toward the readable floor, 1-2 toward the surface).
    fn target(&self, level: Level, surface_l: f64, t: f64) -> Option<f64> {
        let reached = level_target(level, self.appearance, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let extreme = if self.lighter() { 1.0 } else { 0.0 };
        let distance = reached.unwrap_or(extreme) - surface_l;
        let floor =
            level_target(readable_floor(self.appearance), self.appearance, surface_l).unwrap_or(extreme) - surface_l;
        let compressed =
            if distance.abs() > floor.abs() { distance - (distance - floor) * t.min(1.0) } else { distance };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    }

    fn readable(&self, color: RgbColor) -> bool {
        wcag_contrast(self.extreme_text, color) >= TEXT_MINIMUM_WCAG_CONTRAST
    }

    /// Keep a panel light (or dark) enough that white (or black) text still reaches the body text minimum.
    fn limit_panel(&self, token: ThemeToken, l: f64) -> RgbColor {
        let color = self.paint(token, l);
        if self.readable(color) {
            return color;
        }
        let (mut low, mut high) = (self.background_l, l);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if self.readable(self.paint(token, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.paint(token, low)
    }

    fn surface(&self, colors: &TokenMap<Option<RgbColor>>, surface: Surface) -> RgbColor {
        match surface {
            Surface::Background => self.background,
            Surface::Token(token) => colors[token].unwrap_or(self.background),
        }
    }

    fn solve(&self, t: f64) -> Option<TokenMap<Option<RgbColor>>> {
        let mut colors: TokenMap<Option<RgbColor>> = TokenMap::from_fn(|_| None);
        for &token in &self.order {
            let mut targets = Vec::new();
            for rule in self.rules.iter().filter(|rule| rule.token == token) {
                for &surface in &rule.on {
                    let value = self.target(rule.level, oklab_lightness(self.surface(&colors, surface)), t)?;
                    if !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    targets.push(value);
                }
            }
            let l = if self.lighter() {
                targets.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            } else {
                targets.iter().copied().fold(f64::INFINITY, f64::min)
            };
            let color = if is_panel(token) { self.limit_panel(token, l) } else { self.paint(token, l) };
            colors.set(token, Some(color));
        }
        Some(colors)
    }

    fn surfaces_of(&self, colors: &TokenMap<Option<RgbColor>>, token: ThemeToken) -> Vec<RgbColor> {
        let rules = self.rules.iter().filter(|rule| rule.token == token);
        rules.flat_map(|rule| rule.on.iter().map(|&surface| self.surface(colors, surface))).collect()
    }
}

/// Generate the system theme's colors from the terminal's reported colors.
pub fn generate_system_theme_colors(input: &SystemThemeInput) -> SystemThemeColors {
    let saturation = input.saturation.unwrap_or(1.0).clamp(0.0, 1.0);
    let Some(background) = input.background else { return indexed_colors(saturation, input.appearance_hint) };
    let foreground = input.foreground;
    let palette = input.palette.as_ref().filter(|palette| palette.len() == 16);
    let appearance = terminal_appearance(background, foreground);
    let rules = rules();
    let solver = Solver {
        order: solve_order(&rules),
        rules,
        appearance,
        background,
        background_l: oklab_lightness(background),
        palette: palette.map(|palette| palette.iter().copied().map(source_of).collect()),
        saturation,
        extreme_text: if appearance == TerminalTheme::Dark { WHITE } else { BLACK },
    };
    let lighter = solver.lighter();

    let mut relaxation = 0.0;
    let solved = match solver.solve(0.0) {
        Some(colors) => colors,
        None => {
            // Mid-gray backgrounds cannot fit every level: relax as little as possible. Full relaxation fits.
            let (mut low, mut high) = (0.0, 2.0);
            let mut colors = solver.solve(high);
            for _ in 0..20 {
                let middle = (low + high) / 2.0;
                match solver.solve(middle) {
                    Some(attempt) => {
                        high = middle;
                        colors = Some(attempt);
                    }
                    None => low = middle,
                }
            }
            relaxation = high;
            colors.unwrap_or_else(|| TokenMap::from_fn(|_| None))
        }
    };

    let mut result = TokenMap::from_fn(|token| {
        solved[token].map_or(ColorValue::Text(String::new()), |c| ColorValue::Text(hex_of(c)))
    });
    for color in FOREGROUND_TOKENS {
        let token = color.token();
        let surfaces = solver.surfaces_of(&solved, token);
        let mut text = solved[token];
        if let Some(foreground) = foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|surface| solver.target(FOREGROUND_LEVEL, oklab_lightness(*surface), relaxation))
                .collect();
            if targets.iter().all(|value| value.is_some_and(|value| (0.0..=1.0).contains(&value))) {
                let values = targets.into_iter().flatten();
                let needed = if lighter {
                    values.fold(f64::NEG_INFINITY, f64::max)
                } else {
                    values.fold(f64::INFINITY, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                let reaches = if lighter { foreground_l >= needed } else { foreground_l <= needed };
                if reaches {
                    result.set(token, ColorValue::Text(String::new()));
                    continue;
                }
                let lightness = oklab_to_okhsl_lightness(needed);
                text = Some(anchored(source_of(foreground), &NEUTRAL, lightness, saturation));
            }
        }
        if let Some(text) = text {
            result.set(token, ColorValue::Text(hex_of(with_text_contrast(text, &surfaces, lighter))));
        }
    }
    SystemThemeColors { colors: result, dim: Vec::new(), appearance: Some(appearance) }
}
