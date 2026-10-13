//! The pi logo: 4 cells wide and 2 lines tall, drawn with half blocks in fixed brand colors that
//! follow the terminal's color mode (upstream `components/pi-logo.ts`).

use pi_tui::colors::{Color, background_ansi, foreground_ansi, rgb_color};
use pi_tui::is_apple_terminal_session;

use crate::theme::theme;

const RESET: &str = "\x1b[0m";

fn brand(r: f64, g: f64, b: f64) -> Color {
    rgb_color(r, g, b).unwrap_or(Color::Indexed(7))
}

fn coral() -> Color {
    brand(228.0, 138.0, 122.0)
}

fn blue() -> Color {
    brand(79.0, 142.0, 179.0)
}

fn yellow() -> Color {
    brand(234.0, 182.0, 93.0)
}

/// The two logo lines; the top one is padded to the width of the bottom one.
pub fn pi_logo_lines() -> [String; 2] {
    let mode = theme().get_color_mode();
    let fg = |color: Color| foreground_ansi(&color, mode);
    let top = format!("{}{}▀{RESET}{}▀█{RESET} ", fg(coral()), background_ansi(&blue(), mode), fg(coral()));
    let bottom = format!("{}█▀{RESET} {}█{RESET}", fg(blue()), fg(yellow()));
    [top, bottom]
}

/// Apple Terminal draws gaps between rows, so it gets the text wordmark instead.
pub fn supports_pi_logo() -> bool {
    !is_apple_terminal_session()
}

/// Text fallback for the logo: "Pi" in the logo's coral and yellow.
pub fn pi_wordmark() -> String {
    let mode = theme().get_color_mode();
    format!("{}P{RESET}{}i{RESET}", foreground_ansi(&coral(), mode), foreground_ansi(&yellow(), mode))
}
