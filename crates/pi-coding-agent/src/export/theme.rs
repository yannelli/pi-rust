//! Theme colors for the exported page: CSS custom properties plus page, card, and info
//! backgrounds (explicit `export` colors, or derived from `userMessageBg`).

use super::themes::{DARK_COLORS, DARK_EXPORT, LIGHT_COLORS, LIGHT_EXPORT};

/// Resolved theme colors as the export template consumes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportTheme {
    /// Theme tokens to CSS colors, in theme order (`--token: value;`).
    pub colors: Vec<(String, String)>,
    /// The theme's explicit `export.pageBg`, `export.cardBg`, and `export.infoBg`.
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

impl Default for ExportTheme {
    fn default() -> Self {
        Self::dark()
    }
}

impl ExportTheme {
    /// A theme from resolved colors, without explicit export backgrounds.
    pub fn new<K: Into<String>, V: Into<String>>(colors: impl IntoIterator<Item = (K, V)>) -> Self {
        let colors = colors.into_iter().map(|(key, value)| (key.into(), value.into())).collect();
        Self { colors, page_bg: None, card_bg: None, info_bg: None }
    }

    pub fn dark() -> Self {
        Self::from_tables(DARK_COLORS, DARK_EXPORT)
    }

    pub fn light() -> Self {
        Self::from_tables(LIGHT_COLORS, LIGHT_EXPORT)
    }

    /// The built-in `dark` or `light` theme.
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }

    fn from_tables(colors: &[(&str, &str)], export: [&str; 3]) -> Self {
        let [page, card, info] = export.map(|color| Some(color.to_string()));
        Self { page_bg: page, card_bg: card, info_bg: info, ..Self::new(colors.iter().copied()) }
    }

    fn color(&self, key: &str) -> Option<&str> {
        self.colors.iter().find(|(token, _)| token == key).map(|(_, value)| value.as_str())
    }

    /// `pageBg`, `cardBg`, `infoBg`: explicit export colors win over derived ones.
    pub fn backgrounds(&self) -> [String; 3] {
        let user_message_bg = self.color("userMessageBg").filter(|value| !value.is_empty()).unwrap_or("#343541");
        let [page, card, info] = derive_export_colors(user_message_bg);
        [
            self.page_bg.clone().unwrap_or(page),
            self.card_bg.clone().unwrap_or(card),
            self.info_bg.clone().unwrap_or(info),
        ]
    }

    /// The `{{THEME_VARS}}` block (upstream `generateThemeVars`).
    pub fn css_vars(&self) -> String {
        let [page, card, info] = self.backgrounds();
        let mut lines: Vec<String> = self.colors.iter().map(|(key, value)| format!("--{key}: {value};")).collect();
        lines.push(format!("--exportPageBg: {page};"));
        lines.push(format!("--exportCardBg: {card};"));
        lines.push(format!("--exportInfoBg: {info};"));
        lines.join("\n      ")
    }
}

/// Parse `#RRGGBB` or `rgb(r, g, b)`.
fn parse_color(color: &str) -> Option<[f64; 3]> {
    if let Some(hex) = color.strip_prefix('#') {
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok().map(f64::from);
        return Some([channel(0)?, channel(2)?, channel(4)?]);
    }
    let inner = color.strip_prefix("rgb")?.trim_start().strip_prefix('(')?.strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let channel = |part: &str| part.parse::<f64>().ok();
    Some([channel(parts[0])?, channel(parts[1])?, channel(parts[2])?])
}

fn luminance([r, g, b]: [f64; 3]) -> f64 {
    let linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 { s / 12.92 } else { ((s + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some(rgb) = parse_color(color) else { return color.to_string() };
    let [r, g, b] = rgb.map(|c| (c * factor).round().clamp(0.0, 255.0));
    format!("rgb({r}, {g}, {b})")
}

/// Upstream `deriveExportColors`: page, card, and info backgrounds from a base color.
pub fn derive_export_colors(base_color: &str) -> [String; 3] {
    let Some(rgb @ [r, g, b]) = parse_color(base_color) else {
        return ["rgb(24, 24, 30)".into(), "rgb(30, 30, 36)".into(), "rgb(60, 55, 40)".into()];
    };
    if luminance(rgb) > 0.5 {
        let info = format!("rgb({}, {}, {})", (r + 10.0).min(255.0), (g + 5.0).min(255.0), (b - 20.0).max(0.0));
        return [adjust_brightness(base_color, 0.96), base_color.to_string(), info];
    }
    let info = format!("rgb({}, {}, {b})", (r + 20.0).min(255.0), (g + 15.0).min(255.0));
    [adjust_brightness(base_color, 0.7), adjust_brightness(base_color, 0.85), info]
}
