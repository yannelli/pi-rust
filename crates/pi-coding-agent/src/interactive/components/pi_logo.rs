//! The pi logo: 4 cells wide and 2 lines tall, drawn with half blocks in fixed brand colors that
//! follow the terminal's color mode (upstream `components/pi-logo.ts`). pi-rust uses its own
//! palette, the one in `docs/assets/pi-rust-logo.svg`, instead of upstream's coral, blue, and yellow.

use pi_tui::colors::{Color, background_ansi, foreground_ansi, rgb_color};
use pi_tui::is_apple_terminal_session;

use crate::theme::theme;

const RESET: &str = "\x1b[0m";

/// The logo's top bar (rust orange), left leg (steel blue), and right leg (forge amber).
pub const PI_LOGO_COLORS: [[f64; 3]; 3] = [[239.0, 111.0, 58.0], [95.0, 160.0, 191.0], [242.0, 180.0, 76.0]];

fn brand(index: usize) -> Color {
    let [r, g, b] = PI_LOGO_COLORS[index];
    rgb_color(r, g, b).unwrap_or(Color::Indexed(7))
}

fn rust() -> Color {
    brand(0)
}

fn steel() -> Color {
    brand(1)
}

fn amber() -> Color {
    brand(2)
}

/// The two logo lines; the top one is padded to the width of the bottom one.
pub fn pi_logo_lines() -> [String; 2] {
    let mode = theme().get_color_mode();
    let fg = |color: Color| foreground_ansi(&color, mode);
    let top = format!("{}{}▀{RESET}{}▀█{RESET} ", fg(rust()), background_ansi(&steel(), mode), fg(rust()));
    let bottom = format!("{}█▀{RESET} {}█{RESET}", fg(steel()), fg(amber()));
    [top, bottom]
}

/// Apple Terminal draws gaps between rows, so it gets the text wordmark instead.
pub fn supports_pi_logo() -> bool {
    !is_apple_terminal_session()
}

/// Text fallback for the logo: "Pi" in the logo's rust orange and amber.
pub fn pi_wordmark() -> String {
    let mode = theme().get_color_mode();
    format!("{}P{RESET}{}i{RESET}", foreground_ansi(&rust(), mode), foreground_ansi(&amber(), mode))
}
