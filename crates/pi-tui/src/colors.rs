//! Concrete colors (ANSI indexed, sRGB, OKLCH), color math, and ANSI styling.

mod ansi;

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub use ansi::{TextAttributes, TextStyle, background_ansi, foreground_ansi, style_text, style_text_with_ansi};

use crate::oklab::{linear_srgb_to_rgb, okhsl_to_rgb, oklab_to_linear_srgb, rgb_to_okhsl, rgb_to_oklab};
use crate::terminal_colors::{RgbColor, js_round};

/// A concrete color. Every color converts to sRGB, so color math never fails.
/// Build checked values with [`indexed_color`], [`rgb_color`], [`oklch_color`], or [`parse_color`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// An ANSI palette index, 0-255.
    Indexed(u8),
    /// sRGB channels, 0-255. Mixing in sRGB can leave fractional channels.
    Rgb {
        /// Red, 0-255.
        r: f64,
        /// Green, 0-255.
        g: f64,
        /// Blue, 0-255.
        b: f64,
    },
    /// OKLCH: lightness 0-1, chroma 0 or more, hue in degrees 0-360.
    Oklch {
        /// Lightness, 0-1.
        l: f64,
        /// Chroma, 0 or more.
        c: f64,
        /// Hue in degrees, 0-360.
        h: f64,
    },
}

/// How the terminal receives colors: the 256-color palette or 24-bit truecolor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalColorMode {
    /// `38;5;N` palette sequences.
    Color256,
    /// `38;2;R;G;B` sequences.
    Truecolor,
}

/// The color space [`mix_colors`] interpolates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ColorMixSpace {
    /// Perceptual interpolation along the shorter hue arc.
    #[default]
    Oklch,
    /// Channel-wise sRGB interpolation.
    Srgb,
}

/// OKLCH channels: lightness 0-1, chroma, and hue in degrees.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OklchChannels {
    /// Lightness, 0-1.
    pub l: f64,
    /// Chroma, 0 or more.
    pub c: f64,
    /// Hue in degrees.
    pub h: f64,
}

/// OKHSL channels: hue in degrees, saturation and lightness 0-1. Saturation is relative to the
/// most the sRGB gamut allows at that hue and lightness, so every value is in gamut.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OkhslChannels {
    /// Hue in degrees.
    pub h: f64,
    /// Saturation, 0-1.
    pub s: f64,
    /// Lightness, 0-1.
    pub l: f64,
}

/// A color value as a theme file writes it: a string (`#rgb`, `#rrggbb`, `oklch(...)`, `okhsl(...)`)
/// or a number (an ANSI palette index). Upstream `parseColor` takes `string | number`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ColorValue {
    /// A color string.
    Text(String),
    /// An ANSI palette index.
    Number(f64),
}

/// A rejected color value. The message matches the upstream error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColorError(pub String);

impl fmt::Display for ColorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ColorError {}

/// Formats a number the way JavaScript template strings do.
pub(crate) fn js_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    if !(1e-6..1e21).contains(&value.abs()) {
        let formatted = format!("{value:e}");
        return match formatted.split_once('e') {
            Some((mantissa, exponent)) if !exponent.starts_with('-') => format!("{mantissa}e+{exponent}"),
            _ => formatted,
        };
    }
    format!("{value}")
}

fn require_finite(value: f64, name: &str) -> Result<(), ColorError> {
    if value.is_finite() { Ok(()) } else { Err(ColorError(format!("{name} must be finite"))) }
}

/// An ANSI palette color. Fails unless `index` is 0-255.
pub fn indexed_color(index: i64) -> Result<Color, ColorError> {
    u8::try_from(index)
        .map(Color::Indexed)
        .map_err(|_| ColorError(format!("ANSI color index must be an integer from 0 to 255: {index}")))
}

/// An ANSI palette color from a JavaScript-style number. Fails unless it is an integer from 0 to 255.
pub fn indexed_color_from_number(index: f64) -> Result<Color, ColorError> {
    if index.fract() == 0.0 && (0.0..=255.0).contains(&index) {
        return Ok(Color::Indexed(index as u8));
    }
    Err(ColorError(format!("ANSI color index must be an integer from 0 to 255: {}", js_number(index))))
}

/// An sRGB color. Each channel must be finite and 0-255; fractions are kept.
pub fn rgb_color(r: f64, g: f64, b: f64) -> Result<Color, ColorError> {
    for (name, value) in [("r", r), ("g", g), ("b", b)] {
        require_finite(value, name)?;
        if !(0.0..=255.0).contains(&value) {
            return Err(ColorError(format!("{name} must be between 0 and 255: {}", js_number(value))));
        }
    }
    Ok(Color::Rgb { r, g, b })
}

/// An OKLCH color. Lightness must be 0-1 and chroma not negative; hue is normalized to 0-360.
pub fn oklch_color(l: f64, c: f64, h: f64) -> Result<Color, ColorError> {
    require_finite(l, "l")?;
    require_finite(c, "c")?;
    require_finite(h, "h")?;
    if !(0.0..=1.0).contains(&l) {
        return Err(ColorError(format!("l must be between 0 and 1: {}", js_number(l))));
    }
    if c < 0.0 {
        return Err(ColorError(format!("c must not be negative: {}", js_number(c))));
    }
    Ok(Color::Oklch { l, c, h: ((h % 360.0) + 360.0) % 360.0 })
}

/// An OKHSL color converted to sRGB. `h` is in degrees; `s` and `l` are 0-1.
/// Equal saturation looks equally colorful across hues and lightness.
pub fn okhsl_color(h: f64, s: f64, l: f64) -> Result<Color, ColorError> {
    require_finite(h, "h")?;
    require_finite(s, "s")?;
    require_finite(l, "l")?;
    if !(0.0..=1.0).contains(&s) {
        return Err(ColorError(format!("s must be between 0 and 1: {}", js_number(s))));
    }
    if !(0.0..=1.0).contains(&l) {
        return Err(ColorError(format!("l must be between 0 and 1: {}", js_number(l))));
    }
    let RgbColor { r, g, b } = okhsl_to_rgb(h, s, l);
    rgb_color(f64::from(r), f64::from(g), f64::from(b))
}

/// The OKHSL channels of a color.
pub fn color_to_okhsl(color: &Color) -> OkhslChannels {
    rgb_to_okhsl(rgb_channels(color))
}

const NUMBER_PATTERN: &str = r"[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:e[+-]?[0-9]+)?";
static HEX_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^#([0-9a-f]{3}|[0-9a-f]{6})$").expect("valid hex pattern"));
static OKLCH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    let n = NUMBER_PATTERN;
    Regex::new(&format!(r"(?i)^oklch\(\s*({n})(%)?\s+({n})\s+({n})(?:deg)?\s*\)$")).expect("valid oklch pattern")
});
static OKHSL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    let n = NUMBER_PATTERN;
    Regex::new(&format!(r"(?i)^okhsl\(\s*({n})(?:deg)?\s+({n})(%)?\s+({n})(%)?\s*\)$")).expect("valid okhsl pattern")
});

fn parse_float(text: &str) -> f64 {
    text.parse().unwrap_or(f64::NAN)
}

fn percent_divisor(marker: Option<regex::Match<'_>>) -> f64 {
    if marker.is_some() { 100.0 } else { 1.0 }
}

/// Parses a color string: `#rgb`, `#rrggbb`, `oklch(L[%] C H[deg])`, or `okhsl(H[deg] S[%] L[%])`.
/// Fails with `Invalid color value: ...` for anything else.
pub fn parse_color(value: &str) -> Result<Color, ColorError> {
    if let Some(hex) = HEX_PATTERN.captures(value) {
        let digits = &hex[1];
        let digits: String = if digits.len() == 3 {
            digits.chars().flat_map(|digit| [digit, digit]).collect()
        } else {
            digits.to_owned()
        };
        let channel = |start: usize| f64::from(u8::from_str_radix(&digits[start..start + 2], 16).unwrap_or(0));
        return rgb_color(channel(0), channel(2), channel(4));
    }

    if let Some(oklch) = OKLCH_PATTERN.captures(value) {
        let lightness = parse_float(&oklch[1]) / percent_divisor(oklch.get(2));
        return oklch_color(lightness, parse_float(&oklch[3]), parse_float(&oklch[4]));
    }

    if let Some(okhsl) = OKHSL_PATTERN.captures(value) {
        let saturation = parse_float(&okhsl[2]) / percent_divisor(okhsl.get(3));
        let lightness = parse_float(&okhsl[4]) / percent_divisor(okhsl.get(5));
        return okhsl_color(parse_float(&okhsl[1]), saturation, lightness);
    }

    Err(ColorError(format!("Invalid color value: {value}")))
}

/// Parses a theme color value: strings go through [`parse_color`], numbers through [`indexed_color_from_number`].
pub fn parse_color_value(value: &ColorValue) -> Result<Color, ColorError> {
    match value {
        ColorValue::Text(text) => parse_color(text),
        ColorValue::Number(number) => indexed_color_from_number(*number),
    }
}

const BASIC_COLORS: [[u8; 3]; 16] = [
    [0, 0, 0],
    [128, 0, 0],
    [0, 128, 0],
    [128, 128, 0],
    [0, 0, 128],
    [128, 0, 128],
    [0, 128, 128],
    [192, 192, 192],
    [128, 128, 128],
    [255, 0, 0],
    [0, 255, 0],
    [255, 255, 0],
    [0, 0, 255],
    [255, 0, 255],
    [0, 255, 255],
    [255, 255, 255],
];
const CUBE_VALUES: [f64; 6] = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];

fn indexed_to_rgb(index: u8) -> [f64; 3] {
    let index = usize::from(index);
    if index < 16 {
        return BASIC_COLORS[index].map(f64::from);
    }
    if index < 232 {
        let cube = index - 16;
        return [CUBE_VALUES[cube / 36], CUBE_VALUES[(cube % 36) / 6], CUBE_VALUES[cube % 6]];
    }
    [8.0 + (index - 232) as f64 * 10.0; 3]
}

fn is_in_srgb_gamut(linear: &[f64; 3]) -> bool {
    let epsilon = 1e-7;
    linear.iter().all(|&channel| channel >= -epsilon && channel <= 1.0 + epsilon)
}

fn oklch_to_rgb(l: f64, c: f64, h: f64) -> RgbColor {
    // Gamut mapping keeps the hue fixed, so its direction is computed once and scaled by chroma.
    let radians = h * std::f64::consts::PI / 180.0;
    let (sin, cos) = (radians.sin(), radians.cos());
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([l, chroma * cos, chroma * sin]);

    let direct = at_chroma(c);
    if is_in_srgb_gamut(&direct) {
        return linear_srgb_to_rgb(direct);
    }

    // Reduce chroma until the color fits. The achromatic color is always in gamut, so it is the
    // fallback when no bisection step fits; for example `oklch(100% 0.3 150)` maps to white.
    let mut linear = at_chroma(0.0);
    let (mut low, mut high) = (0.0, c);
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if is_in_srgb_gamut(&candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

/// sRGB channels as floats, keeping the fractions that sRGB mixing produces.
pub(crate) fn rgb_channels(color: &Color) -> [f64; 3] {
    match *color {
        Color::Rgb { r, g, b } => [r, g, b],
        Color::Indexed(index) => indexed_to_rgb(index),
        Color::Oklch { l, c, h } => {
            let rgb = oklch_to_rgb(l, c, h);
            [f64::from(rgb.r), f64::from(rgb.g), f64::from(rgb.b)]
        }
    }
}

fn channel_u8(value: f64) -> u8 {
    js_round(value).clamp(0.0, 255.0) as u8
}

/// The sRGB value of a color. OKLCH colors are gamut-mapped; fractional sRGB channels are rounded.
pub fn color_to_rgb(color: &Color) -> RgbColor {
    let [r, g, b] = rgb_channels(color);
    RgbColor { r: channel_u8(r), g: channel_u8(g), b: channel_u8(b) }
}

/// The OKLCH channels of a color.
pub fn color_to_oklch(color: &Color) -> OklchChannels {
    if let Color::Oklch { l, c, h } = *color {
        return OklchChannels { l, c, h };
    }
    let [l, a, b] = rgb_to_oklab(rgb_channels(color));
    OklchChannels { l, c: a.hypot(b), h: (b.atan2(a) * 180.0 / std::f64::consts::PI + 360.0) % 360.0 }
}

/// The `#rrggbb` form of a color.
pub fn color_to_hex(color: &Color) -> String {
    let RgbColor { r, g, b } = color_to_rgb(color);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// Interpolates from `first` (amount 0) to `second` (amount 1). Fails unless `amount` is 0-1.
pub fn mix_colors(first: &Color, second: &Color, amount: f64, space: ColorMixSpace) -> Result<Color, ColorError> {
    require_finite(amount, "amount")?;
    if !(0.0..=1.0).contains(&amount) {
        return Err(ColorError(format!("amount must be between 0 and 1: {}", js_number(amount))));
    }

    if space == ColorMixSpace::Srgb {
        let a = rgb_channels(first);
        let b = rgb_channels(second);
        let mix = |index: usize| a[index] + (b[index] - a[index]) * amount;
        return rgb_color(mix(0), mix(1), mix(2));
    }

    let a = color_to_oklch(first);
    let b = color_to_oklch(second);
    let first_hue = if a.c < 1e-7 { b.h } else { a.h };
    let second_hue = if b.c < 1e-7 { first_hue } else { b.h };
    let hue_delta = ((second_hue - first_hue + 540.0) % 360.0) - 180.0;
    oklch_color(a.l + (b.l - a.l) * amount, a.c + (b.c - a.c) * amount, first_hue + hue_delta * amount)
}
