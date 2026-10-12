//! Tracking of active SGR attributes and OSC 8 links, so styling survives line breaks.

use super::ansi::ansi_code_length;
use super::osc8::{ActiveHyperlink, Osc8, format_osc8_close, format_osc8_hyperlink, parse_osc8_hyperlink};

/// Tracks the SGR attributes, colors, and OSC 8 hyperlink that are active after a run of escape codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnsiCodeTracker {
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    hidden: bool,
    strikethrough: bool,
    fg_color: Option<String>,
    bg_color: Option<String>,
    active_hyperlink: Option<ActiveHyperlink>,
}

/// Parameters of the first `ESC [ <digits and semicolons> m` sequence in `code`.
fn sgr_params(code: &str) -> Option<&str> {
    let bytes = code.as_bytes();
    for start in 0..bytes.len().saturating_sub(1) {
        if bytes[start] != 0x1b || bytes[start + 1] != b'[' {
            continue;
        }
        let params_start = start + 2;
        let params_len = bytes[params_start..].iter().take_while(|b| b.is_ascii_digit() || **b == b';').count();
        if bytes.get(params_start + params_len) == Some(&b'm') {
            return Some(&code[params_start..params_start + params_len]);
        }
    }
    None
}

impl AnsiCodeTracker {
    /// Creates a tracker with no active attributes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the state from one escape sequence (SGR or OSC 8); other sequences are ignored.
    pub fn process(&mut self, ansi_code: &str) {
        // Keep the original OSC 8 terminator: some terminals only make BEL-terminated links clickable.
        match parse_osc8_hyperlink(ansi_code) {
            Osc8::Open(link) => {
                self.active_hyperlink = Some(link);
                return;
            }
            Osc8::Close => {
                self.active_hyperlink = None;
                return;
            }
            Osc8::Other => {}
        }
        if !ansi_code.ends_with('m') {
            return;
        }
        let Some(params) = sgr_params(ansi_code) else {
            return;
        };
        if params.is_empty() || params == "0" {
            self.reset();
            return;
        }

        let parts: Vec<&str> = params.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let code = parts[i].parse::<u64>().ok();
            if matches!(code, Some(38 | 48)) {
                let color = if parts.get(i + 1) == Some(&"5") && i + 2 < parts.len() {
                    Some((parts[i..i + 3].join(";"), 3))
                } else if parts.get(i + 1) == Some(&"2") && i + 4 < parts.len() {
                    Some((parts[i..i + 5].join(";"), 5))
                } else {
                    None
                };
                if let Some((color, consumed)) = color {
                    if code == Some(38) {
                        self.fg_color = Some(color);
                    } else {
                        self.bg_color = Some(color);
                    }
                    i += consumed;
                    continue;
                }
            }
            match code {
                Some(0) => self.reset(),
                Some(1) => self.bold = true,
                Some(2) => self.dim = true,
                Some(3) => self.italic = true,
                Some(4) => self.underline = true,
                Some(5) => self.blink = true,
                Some(7) => self.inverse = true,
                Some(8) => self.hidden = true,
                Some(9) => self.strikethrough = true,
                Some(21) => self.bold = false,
                Some(22) => {
                    self.bold = false;
                    self.dim = false;
                }
                Some(23) => self.italic = false,
                Some(24) => self.underline = false,
                Some(25) => self.blink = false,
                Some(27) => self.inverse = false,
                Some(28) => self.hidden = false,
                Some(29) => self.strikethrough = false,
                Some(39) => self.fg_color = None,
                Some(49) => self.bg_color = None,
                Some(n @ (30..=37 | 90..=97)) => self.fg_color = Some(n.to_string()),
                Some(n @ (40..=47 | 100..=107)) => self.bg_color = Some(n.to_string()),
                _ => {}
            }
            i += 1;
        }
    }

    /// SGR reset; the OSC 8 link state is not affected.
    fn reset(&mut self) {
        let link = self.active_hyperlink.take();
        *self = Self { active_hyperlink: link, ..Self::default() };
    }

    /// Clears all state, including the active hyperlink.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Escape codes that re-establish the current state (SGR first, then the open hyperlink).
    pub fn get_active_codes(&self) -> String {
        let flags = [
            (self.bold, "1"),
            (self.dim, "2"),
            (self.italic, "3"),
            (self.underline, "4"),
            (self.blink, "5"),
            (self.inverse, "7"),
            (self.hidden, "8"),
            (self.strikethrough, "9"),
        ];
        let mut codes: Vec<&str> = flags.iter().filter(|(on, _)| *on).map(|(_, code)| *code).collect();
        codes.extend(self.fg_color.as_deref());
        codes.extend(self.bg_color.as_deref());
        let mut result = if codes.is_empty() { String::new() } else { format!("\x1b[{}m", codes.join(";")) };
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8_hyperlink(link));
        }
        result
    }

    /// The SGR code for the active background color, or an empty string.
    pub fn get_active_background_code(&self) -> String {
        self.bg_color.as_ref().map_or_else(String::new, |bg| format!("\x1b[{bg}m"))
    }

    /// True when any attribute, color, or hyperlink is active.
    pub fn has_active_codes(&self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg_color.is_some()
            || self.bg_color.is_some()
            || self.active_hyperlink.is_some()
    }

    /// Codes to emit at a line end: underline off (so it does not bleed into padding) and a hyperlink close.
    pub fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if let Some(link) = &self.active_hyperlink {
            result.push_str(&format_osc8_close(link.terminator));
        }
        result
    }

    /// Processes every escape sequence found in `text`.
    pub fn process_text(&mut self, text: &str) {
        let mut search = 0;
        while let Some(offset) = text[search..].find('\x1b') {
            let escape = search + offset;
            let length = ansi_code_length(text, escape);
            if length > 0 {
                self.process(&text[escape..escape + length]);
                search = escape + length;
            } else {
                search = escape + 1;
            }
        }
    }
}

/// Returns only the background color code active at the end of an ANSI-styled string.
pub fn get_active_background_ansi(text: &str) -> String {
    let mut tracker = AnsiCodeTracker::new();
    tracker.process_text(text);
    tracker.get_active_background_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_sgr_attributes_and_colors() {
        let mut tracker = AnsiCodeTracker::new();
        tracker.process_text("\x1b[1;4m\x1b[38;5;240m\x1b[48;2;1;2;3mx\x1b[22m");
        assert_eq!(tracker.get_active_codes(), "\x1b[4;38;5;240;48;2;1;2;3m");
        assert_eq!(tracker.get_line_end_reset(), "\x1b[24m");
        tracker.process("\x1b[0m");
        assert!(!tracker.has_active_codes());
        assert_eq!(get_active_background_ansi("\x1b[41mred\x1b[33m"), "\x1b[41m");
        assert_eq!(get_active_background_ansi("\x1b[41m\x1b[49m"), "");
    }

    #[test]
    fn keeps_hyperlink_across_sgr_reset() {
        let mut tracker = AnsiCodeTracker::new();
        tracker.process("\x1b]8;;https://a\x07");
        tracker.process("\x1b[31m");
        tracker.process("\x1b[m");
        assert_eq!(tracker.get_active_codes(), "\x1b]8;;https://a\x07");
        assert_eq!(tracker.get_line_end_reset(), "\x1b]8;;\x07");
        tracker.clear();
        assert!(!tracker.has_active_codes());
    }

    #[test]
    fn handles_odd_parameters_like_upstream() {
        let mut tracker = AnsiCodeTracker::new();
        tracker.process("\x1b[38;05;1m");
        assert_eq!(tracker.get_active_codes(), "\x1b[1;5m");
        tracker.process("\x1b[;031m");
        assert_eq!(tracker.get_active_codes(), "\x1b[1;5;31m");
        tracker.process("\x1b[22;38;5m");
        assert_eq!(tracker.get_active_codes(), "\x1b[5;31m");
    }
}
