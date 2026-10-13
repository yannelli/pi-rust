//! Terminal color reports: OSC 10/11/4 color replies and color scheme (DSR 997) reports.

use std::sync::LazyLock;

use regex::Regex;

/// An sRGB color with 8-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct RgbColor {
    /// Red channel, 0-255.
    pub r: u8,
    /// Green channel, 0-255.
    pub g: u8,
    /// Blue channel, 0-255.
    pub b: u8,
}

impl RgbColor {
    /// Builds a color from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Whether the terminal theme is dark or light.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalColorScheme {
    /// A dark background theme.
    Dark,
    /// A light background theme.
    Light,
}

/// Colors the terminal reports for its current theme.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// Default foreground (OSC 10).
    pub foreground: Option<RgbColor>,
    /// Default background (OSC 11).
    pub background: Option<RgbColor>,
    /// ANSI colors 0-15 (OSC 4). Only set when the terminal reported all 16.
    pub palette: Option<Vec<RgbColor>>,
}

/// What an OSC color reply reports: the default foreground (OSC 10), background (OSC 11), or a palette index (OSC 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OscColorTarget {
    /// The default foreground color (OSC 10).
    Foreground,
    /// The default background color (OSC 11).
    Background,
    /// A palette entry (OSC 4).
    Palette(u8),
}

/// A parsed OSC color reply. `rgb` is `None` when the reply carries a color that cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OscColorResponse {
    /// The color slot the reply describes.
    pub target: OscColorTarget,
    /// The reported color, when it could be parsed.
    pub rgb: Option<RgbColor>,
}

static OSC_COLOR_RESPONSE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\x1b\](?:(1[01])|4;([0-9]{1,3}));([^\x07\x1b]*)(?:\x07|\x1b\\)$").expect("valid OSC color pattern")
});
static COLOR_SCHEME_REPORT_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:\x1b\[\?997;(1|2)n)+$").expect("valid color scheme pattern"));

/// Rounds like JavaScript `Math.round`: halves round toward positive infinity.
pub(crate) fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 { floor + 1.0 } else { floor }
}

fn is_hex_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex_pair(text: &str) -> u8 {
    u8::from_str_radix(text, 16).unwrap_or(0)
}

fn parse_osc_hex_channel(channel: &str) -> Option<u8> {
    if !is_hex_digits(channel) {
        return None;
    }
    let max = 16f64.powi(i32::try_from(channel.len()).ok()?) - 1.0;
    if max <= 0.0 {
        return None;
    }
    let value = channel.bytes().fold(0f64, |acc, byte| acc * 16.0 + f64::from(hex_value(byte)));
    Some(js_round(value / max * 255.0).clamp(0.0, 255.0) as u8)
}

fn hex_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn parse_osc_color_value(raw_value: &str) -> Option<RgbColor> {
    let value = raw_value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && is_hex_digits(hex) {
            return Some(RgbColor::new(hex_pair(&hex[0..2]), hex_pair(&hex[2..4]), hex_pair(&hex[4..6])));
        }
        if hex.len() == 12 && is_hex_digits(hex) {
            let r = parse_osc_hex_channel(&hex[0..4])?;
            let g = parse_osc_hex_channel(&hex[4..8])?;
            let b = parse_osc_hex_channel(&hex[8..12])?;
            return Some(RgbColor::new(r, g, b));
        }
        return None;
    }

    let rgb_value = strip_rgb_prefix(value);
    let mut parts = rgb_value.split('/');
    let (red, green, blue) = (parts.next()?, parts.next()?, parts.next()?);
    let r = parse_osc_hex_channel(red)?;
    let g = parse_osc_hex_channel(green)?;
    let b = parse_osc_hex_channel(blue)?;
    Some(RgbColor::new(r, g, b))
}

fn strip_rgb_prefix(value: &str) -> &str {
    for prefix in ["rgba:", "rgb:"] {
        if value.len() >= prefix.len()
            && value.is_char_boundary(prefix.len())
            && value[..prefix.len()].eq_ignore_ascii_case(prefix)
        {
            return &value[prefix.len()..];
        }
    }
    value
}

/// Parses an OSC 10, 11, or 4 color reply.
/// Returns `None` when `data` is not such a reply (palette indices above 255 included);
/// `rgb` is `None` when it is a reply with an unparseable color.
pub fn parse_osc_color_response(data: &str) -> Option<OscColorResponse> {
    let captures = OSC_COLOR_RESPONSE_PATTERN.captures(data)?;
    let target = match captures.get(1).map(|m| m.as_str()) {
        Some("10") => OscColorTarget::Foreground,
        Some(_) => OscColorTarget::Background,
        None => OscColorTarget::Palette(captures.get(2)?.as_str().parse().ok()?),
    };
    let value = captures.get(3).map_or("", |m| m.as_str());
    Some(OscColorResponse { target, rgb: parse_osc_color_value(value) })
}

/// Parses one or more color scheme reports (`CSI ? 997 ; 1|2 n`). The last report wins.
pub fn parse_terminal_color_scheme_report(data: &str) -> Option<TerminalColorScheme> {
    let captures = COLOR_SCHEME_REPORT_PATTERN.captures(data)?;
    match captures.get(1).map(|m| m.as_str()) {
        Some("2") => Some(TerminalColorScheme::Light),
        _ => Some(TerminalColorScheme::Dark),
    }
}
