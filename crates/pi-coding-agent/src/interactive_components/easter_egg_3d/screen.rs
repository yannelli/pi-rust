//! Parses the captured screen into colored cells that dissolve into braille dust.

use pi_tui::colors::indexed_color_from_number;
use pi_tui::visible_width;
use unicode_segmentation::UnicodeSegmentation;

use super::math::{Rgb, mix, to_rgb};

/// One terminal cell of the captured screen.
#[derive(Debug, Clone, PartialEq)]
pub struct ScreenCell {
    pub text: String,
    /// 1 or 2 for a grapheme, 0 for the second column of a wide grapheme.
    pub width: usize,
    pub fg: Rgb,
    /// `None` when the cell uses the terminal's default background.
    pub bg: Option<Rgb>,
    /// Seconds until the cell turns into braille dust.
    pub delay: f64,
    /// Number of braille dots the glyph turns into.
    pub ink: usize,
    pub seed: i64,
}

/// Braille dots a glyph turns into, a rough measure of how much ink it has.
pub fn glyph_ink(text: &str, seed: f64) -> usize {
    if text.trim().is_empty() {
        return 0;
    }
    let mut chars = text.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ".,:;'`-_·".contains(ch) {
            return 2;
        }
        if ('\u{2500}'..='\u{257f}').contains(&ch) {
            return 3;
        }
    }
    4 + (seed * 3.0).floor() as usize
}

/// `Number.parseInt(text, 10)`: leading digits with an optional sign, NaN without any.
fn parse_int(text: &str) -> f64 {
    let text = text.trim_start();
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'-') => (-1.0, &text[1..]),
        Some(b'+') => (1.0, &text[1..]),
        _ => (1.0, text),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return f64::NAN;
    }
    digits[..end].parse::<f64>().map_or(f64::NAN, |value| sign * value)
}

fn palette(index: f64) -> Option<Rgb> {
    indexed_color_from_number(index).ok().map(|color| to_rgb(&color))
}

#[derive(Default)]
struct SgrState {
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    dim: bool,
    inverse: bool,
}

impl SgrState {
    fn apply(&mut self, params: &str) {
        let codes: Vec<&str> = if params.is_empty() { vec!["0"] } else { params.split(';').collect() };
        let mut i = 0;
        while i < codes.len() {
            let parts: Vec<f64> = codes[i].split(':').map(parse_int).collect();
            let code = parts[0];
            if code == 38.0 || code == 48.0 {
                // Extended color, either as `38;5;n` / `38;2;r;g;b` or colon sub-parameters.
                let args: Vec<f64> = if parts.len() > 1 {
                    // `38:2::r:g:b` has an empty color space id, dropped here.
                    parts[1..].iter().copied().filter(|part| !part.is_nan()).collect()
                } else {
                    let kind = codes.get(i + 1).map_or(f64::NAN, |code| parse_int(code));
                    let count = if kind == 5.0 {
                        2
                    } else if kind == 2.0 {
                        4
                    } else {
                        1
                    };
                    let end = (i + 1 + count).min(codes.len());
                    let args = codes.get(i + 1..end).unwrap_or_default().iter().map(|code| parse_int(code)).collect();
                    i += count;
                    args
                };
                let color = match args.first() {
                    Some(&kind) if kind == 5.0 && args.len() > 1 => palette(args[1]),
                    Some(&kind) if kind == 2.0 && args.len() >= 4 => Some([args[1], args[2], args[3]]),
                    _ => None,
                };
                if let Some(color) = color {
                    if code == 38.0 {
                        self.fg = Some(color);
                    } else {
                        self.bg = Some(color);
                    }
                }
            } else if code == 0.0 {
                *self = Self::default();
            } else if code == 2.0 {
                self.dim = true;
            } else if code == 22.0 {
                self.dim = false;
            } else if code == 7.0 {
                self.inverse = true;
            } else if code == 27.0 {
                self.inverse = false;
            } else if (30.0..=37.0).contains(&code) {
                self.fg = palette(code - 30.0);
            } else if (90.0..=97.0).contains(&code) {
                self.fg = palette(code - 90.0 + 8.0);
            } else if code == 39.0 {
                self.fg = None;
            } else if (40.0..=47.0).contains(&code) {
                self.bg = palette(code - 40.0);
            } else if (100.0..=107.0).contains(&code) {
                self.bg = palette(code - 100.0 + 8.0);
            } else if code == 49.0 {
                self.bg = None;
            }
            i += 1;
        }
    }
}

fn push_text(cells: &mut Vec<ScreenCell>, text: &str, state: &SgrState, width: usize, colors: (Rgb, Rgb)) {
    let (foreground, background) = colors;
    for segment in text.graphemes(true) {
        let glyph_width = visible_width(segment);
        if glyph_width == 0 || cells.len() + glyph_width > width {
            continue;
        }
        let mut fg = state.fg.unwrap_or(foreground);
        let mut bg = state.bg;
        if state.inverse {
            fg = state.bg.unwrap_or(background);
            bg = Some(state.fg.unwrap_or(foreground));
        }
        if state.dim {
            fg = mix(fg, bg.unwrap_or(background), 0.4);
        }
        let cell = ScreenCell { text: segment.to_string(), width: glyph_width, fg, bg, delay: 0.0, ink: 0, seed: 0 };
        let wide = glyph_width == 2;
        cells.push(cell.clone());
        if wide {
            cells.push(ScreenCell { text: String::new(), width: 0, ..cell });
        }
    }
}

fn parse_line(line: &str, width: usize, colors: (Rgb, Rgb)) -> Vec<ScreenCell> {
    let mut cells = Vec::new();
    let mut state = SgrState::default();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < line.len() {
        let Some(offset) = line[i..].find('\x1b') else {
            push_text(&mut cells, &line[i..], &state, width, colors);
            break;
        };
        let start = i + offset;
        if start > i {
            push_text(&mut cells, &line[i..start], &state, width, colors);
        }
        match bytes.get(start + 1) {
            Some(b'[') => {
                let mut end = start + 2;
                while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                    end += 1;
                }
                if bytes.get(end) == Some(&b'm') {
                    state.apply(&line[start + 2..end]);
                }
                i = end + 1;
            }
            Some(b']' | b'_' | b'P' | b'^') => {
                // String sequences end at BEL or ST (ESC \).
                let rest = &line[start + 2..];
                let bell = rest.find('\x07');
                let st = rest.find("\x1b\\");
                i = match (bell, st) {
                    (None, None) => break,
                    (Some(bell), None) => start + 2 + bell + 1,
                    (Some(bell), Some(st)) if bell < st => start + 2 + bell + 1,
                    (_, Some(st)) => start + 2 + st + 2,
                };
            }
            Some(_) => {
                let next = line[start + 1..].chars().next().map_or(1, char::len_utf8);
                i = start + 1 + next;
            }
            None => i = start + 2,
        }
    }
    cells
}

/// Parses rendered lines into cells with resolved colors. Other escape sequences (OSC, APC, cursor) are skipped.
pub fn parse_screen(lines: &[String], width: usize, foreground: Rgb, background: Rgb) -> Vec<Vec<ScreenCell>> {
    lines.iter().map(|line| parse_line(line, width, (foreground, background))).collect()
}
