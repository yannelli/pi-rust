//! SGR escape sequences for colors and text attributes.

use super::{CUBE_VALUES, Color, TerminalColorMode, rgb_channels};
use crate::terminal_colors::js_round;

/// Text attributes without colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TextAttributes {
    /// SGR 1.
    pub bold: bool,
    /// SGR 2.
    pub dim: bool,
    /// SGR 3.
    pub italic: bool,
    /// SGR 4.
    pub underline: bool,
    /// SGR 7.
    pub inverse: bool,
    /// SGR 9.
    pub strikethrough: bool,
}

/// Text attributes plus optional foreground and background colors.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TextStyle {
    /// Foreground color.
    pub fg: Option<Color>,
    /// Background color.
    pub bg: Option<Color>,
    /// SGR 1.
    pub bold: bool,
    /// SGR 2.
    pub dim: bool,
    /// SGR 3.
    pub italic: bool,
    /// SGR 4.
    pub underline: bool,
    /// SGR 7.
    pub inverse: bool,
    /// SGR 9.
    pub strikethrough: bool,
}

impl TextStyle {
    /// The attributes of this style, without its colors.
    pub fn attributes(&self) -> TextAttributes {
        TextAttributes {
            bold: self.bold,
            dim: self.dim,
            italic: self.italic,
            underline: self.underline,
            inverse: self.inverse,
            strikethrough: self.strikethrough,
        }
    }
}

fn find_closest(values: impl IntoIterator<Item = f64>, target: f64) -> usize {
    let mut closest_index = 0;
    let mut closest_distance = f64::INFINITY;
    for (index, value) in values.into_iter().enumerate() {
        let distance = (target - value).abs();
        if distance < closest_distance {
            closest_index = index;
            closest_distance = distance;
        }
    }
    closest_index
}

fn color_distance(first: [f64; 3], second: [f64; 3]) -> f64 {
    let dr = first[0] - second[0];
    let dg = first[1] - second[1];
    let db = first[2] - second[2];
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

fn gray_value(offset: usize) -> f64 {
    8.0 + offset as f64 * 10.0
}

fn rgb_to_ansi256(color: [f64; 3]) -> usize {
    let [r_index, g_index, b_index] = color.map(|channel| find_closest(CUBE_VALUES, channel));
    let cube_color = [CUBE_VALUES[r_index], CUBE_VALUES[g_index], CUBE_VALUES[b_index]];
    let cube_index = 16 + 36 * r_index + 6 * g_index + b_index;

    let gray = js_round(0.299 * color[0] + 0.587 * color[1] + 0.114 * color[2]);
    let gray_offset = find_closest((0..24).map(gray_value), gray);
    let gray = gray_value(gray_offset);
    let spread = color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2]);
    if spread < 10.0 && color_distance(color, [gray; 3]) < color_distance(color, cube_color) {
        return 232 + gray_offset;
    }
    cube_index
}

fn color_ansi(color: &Color, mode: TerminalColorMode, background: bool) -> String {
    let layer = if background { 48 } else { 38 };
    if let Color::Indexed(index) = color {
        return format!("\x1b[{layer};5;{index}m");
    }
    let rgb = rgb_channels(color);
    if mode == TerminalColorMode::Truecolor {
        let [r, g, b] = rgb.map(js_round);
        return format!("\x1b[{layer};2;{r};{g};{b}m");
    }
    format!("\x1b[{layer};5;{}m", rgb_to_ansi256(rgb))
}

/// The SGR sequence that sets `color` as the foreground.
pub fn foreground_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, false)
}

/// The SGR sequence that sets `color` as the background.
pub fn background_ansi(color: &Color, mode: TerminalColorMode) -> String {
    color_ansi(color, mode, true)
}

/// Wraps `text` in the sequences for `style`, closing them in reverse order.
pub fn style_text(text: &str, style: &TextStyle, mode: TerminalColorMode) -> String {
    let fg = style.fg.map(|color| foreground_ansi(&color, mode));
    let bg = style.bg.map(|color| background_ansi(&color, mode));
    style_text_with_ansi(text, fg.as_deref(), bg.as_deref(), &style.attributes())
}

/// Like [`style_text`], but with precomputed color sequences, such as cached theme colors.
/// Empty sequences count as absent.
pub fn style_text_with_ansi(
    text: &str,
    fg_ansi: Option<&str>,
    bg_ansi: Option<&str>,
    options: &TextAttributes,
) -> String {
    // Resets are prepended so they close in reverse order of the opening sequences.
    let mut prefix = String::new();
    let mut suffix = String::new();
    if let Some(fg) = fg_ansi.filter(|ansi| !ansi.is_empty()) {
        prefix.push_str(fg);
        suffix.push_str("\x1b[39m");
    }
    if let Some(bg) = bg_ansi.filter(|ansi| !ansi.is_empty()) {
        prefix.push_str(bg);
        suffix.insert_str(0, "\x1b[49m");
    }
    if options.bold {
        prefix.push_str("\x1b[1m");
    }
    if options.dim {
        prefix.push_str("\x1b[2m");
    }
    if options.bold || options.dim {
        suffix.insert_str(0, "\x1b[22m");
    }
    for (enabled, open, close) in [
        (options.italic, "\x1b[3m", "\x1b[23m"),
        (options.underline, "\x1b[4m", "\x1b[24m"),
        (options.inverse, "\x1b[7m", "\x1b[27m"),
        (options.strikethrough, "\x1b[9m", "\x1b[29m"),
    ] {
        if enabled {
            prefix.push_str(open);
            suffix.insert_str(0, close);
        }
    }
    format!("{prefix}{text}{suffix}")
}
