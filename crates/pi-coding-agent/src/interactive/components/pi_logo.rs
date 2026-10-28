//! The pi logo: 4 cells wide and 2 lines tall, drawn with half blocks in fixed brand colors that
//! follow the terminal's color mode (upstream `components/pi-logo.ts`). pi-rust draws the same
//! glyph in rust tones instead of upstream's coral, blue, and yellow.

use pi_tui::colors::{Color, background_ansi, foreground_ansi, rgb_color};
use pi_tui::is_apple_terminal_session;

use crate::theme::theme;

const RESET: &str = "\x1b[0m";

/// The logo's top bar (rust orange), left leg (oxide), and right leg (copper).
pub const PI_LOGO_COLORS: [[f64; 3]; 3] = [[222.0, 94.0, 40.0], [163.0, 59.0, 26.0], [230.0, 152.0, 79.0]];

/// Width of the logo in cells.
pub const PI_LOGO_WIDTH: usize = 4;
/// Height of the logo in lines.
pub const PI_LOGO_HEIGHT: usize = 2;

/// The glyph's pixels: `t` top bar, `l` left leg, `r` right leg.
const PI_PIXELS: [&str; 4] = ["ttt.", "l.t.", "ll.r", "l..r"];

fn brand(index: usize) -> Color {
    let [r, g, b] = PI_LOGO_COLORS[index];
    rgb_color(r, g, b).unwrap_or(Color::Indexed(7))
}

fn rust() -> Color {
    brand(0)
}

fn oxide() -> Color {
    brand(1)
}

fn copper() -> Color {
    brand(2)
}

/// The two logo lines; the top one is padded to the width of the bottom one.
pub fn pi_logo_lines() -> [String; PI_LOGO_HEIGHT] {
    let mode = theme().get_color_mode();
    let fg = |color: Color| foreground_ansi(&color, mode);
    let top = format!("{}{}▀{RESET}{}▀█{RESET} ", fg(rust()), background_ansi(&oxide(), mode), fg(rust()));
    let bottom = format!("{}█▀{RESET} {}█{RESET}", fg(oxide()), fg(copper()));
    [top, bottom]
}

/// The glyph in full blocks, two columns per pixel, for first-time setup. Full blocks also line
/// up in Apple Terminal.
pub fn pi_logo_large_lines() -> [String; 4] {
    let mode = theme().get_color_mode();
    PI_PIXELS.map(|row| {
        let cells: Vec<String> = row
            .bytes()
            .map(|pixel| match pixel {
                b't' => format!("{}██{RESET}", foreground_ansi(&rust(), mode)),
                b'l' => format!("{}██{RESET}", foreground_ansi(&oxide(), mode)),
                b'r' => format!("{}██{RESET}", foreground_ansi(&copper(), mode)),
                _ => "  ".to_string(),
            })
            .collect();
        cells.concat().trim_end().to_string()
    })
}

/// Apple Terminal draws gaps between rows, so it gets the text wordmark instead.
pub fn supports_pi_logo() -> bool {
    !is_apple_terminal_session()
}

/// Text fallback for the logo: "Pi" in the logo's rust orange and copper.
pub fn pi_wordmark() -> String {
    let mode = theme().get_color_mode();
    format!("{}P{RESET}{}i{RESET}", foreground_ansi(&rust(), mode), foreground_ansi(&copper(), mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive_components::test_support::plain;
    use crate::theme::tests::global_state;

    #[test]
    fn draws_the_pi_glyph_in_rust_tones() {
        let _theme = global_state();
        assert_eq!(plain(&pi_logo_lines()), ["▀▀█ ", "█▀ █"]);
        assert!(pi_logo_lines()[0].contains("\x1b[38;2;222;94;40m") || pi_logo_lines()[0].contains("\x1b[38;5;"));
        assert_eq!(plain(&pi_logo_large_lines()), ["██████", "██  ██", "████  ██", "██    ██"]);
    }
}
