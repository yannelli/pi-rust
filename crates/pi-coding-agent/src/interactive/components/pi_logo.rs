//! The pi-rust logo: the pi glyph on dark iron inside a gear, 8x8 pixels drawn with half blocks in fixed
//! brand colors that follow the terminal's color mode. Upstream `components/pi-logo.ts` draws only the
//! 4x4 pi glyph; this one matches `docs/assets/pi-rust-logo.svg`.

use pi_tui::colors::{Color, background_ansi, foreground_ansi, rgb_color};
use pi_tui::is_apple_terminal_session;

use crate::theme::theme;

const RESET: &str = "\x1b[0m";

/// The pi glyph's top bar (rust orange), left leg (steel blue), and right leg (forge amber).
pub const PI_LOGO_COLORS: [[f64; 3]; 3] = [[239.0, 111.0, 58.0], [95.0, 160.0, 191.0], [242.0, 180.0, 76.0]];
const GEAR: [f64; 3] = [196.0, 85.0, 42.0];
const IRON: [f64; 3] = [35.0, 26.0, 22.0];

/// Width of the logo in cells.
pub const PI_LOGO_WIDTH: usize = 8;
/// Height of the logo in lines.
pub const PI_LOGO_HEIGHT: usize = 4;
/// Cell column and line of the pi glyph inside the logo, where the 3D easter egg lifts it off.
pub const PI_GLYPH_OFFSET: (usize, usize) = (2, 1);

// g gear, i iron, and the pi glyph's t top bar, l left leg, r right leg.
const PIXELS: [&str; PI_LOGO_HEIGHT * 2] =
    ["...gg...", ".gggggg.", ".gtttig.", "gglitigg", "ggllirgg", ".gliirg.", ".gggggg.", "...gg..."];

fn brand([r, g, b]: [f64; 3]) -> Color {
    rgb_color(r, g, b).unwrap_or(Color::Indexed(7))
}

fn pixel_color(pixel: u8) -> Option<Color> {
    let rgb = match pixel {
        b'g' => GEAR,
        b'i' => IRON,
        b't' => PI_LOGO_COLORS[0],
        b'l' => PI_LOGO_COLORS[1],
        b'r' => PI_LOGO_COLORS[2],
        _ => return None,
    };
    Some(brand(rgb))
}

/// The logo lines, each [`PI_LOGO_WIDTH`] cells wide.
pub fn pi_logo_lines() -> [String; PI_LOGO_HEIGHT] {
    let mode = theme().get_color_mode();
    let fg = |color: Color| foreground_ansi(&color, mode);
    std::array::from_fn(|line| {
        let (top, bottom) = (PIXELS[line * 2].as_bytes(), PIXELS[line * 2 + 1].as_bytes());
        (0..PI_LOGO_WIDTH)
            .map(|column| match (pixel_color(top[column]), pixel_color(bottom[column])) {
                (None, None) => " ".to_string(),
                (Some(upper), None) => format!("{}▀{RESET}", fg(upper)),
                (None, Some(lower)) => format!("{}▄{RESET}", fg(lower)),
                (Some(upper), Some(_)) if top[column] == bottom[column] => format!("{}█{RESET}", fg(upper)),
                (Some(upper), Some(lower)) => format!("{}{}▀{RESET}", fg(upper), background_ansi(&lower, mode)),
            })
            .collect()
    })
}

/// Apple Terminal draws gaps between rows, so it gets the text wordmark instead.
pub fn supports_pi_logo() -> bool {
    !is_apple_terminal_session()
}

/// Text fallback for the logo: "Pi" in the logo's rust orange and amber.
pub fn pi_wordmark() -> String {
    let mode = theme().get_color_mode();
    let [top_bar, _, right_leg] = PI_LOGO_COLORS;
    format!("{}P{RESET}{}i{RESET}", foreground_ansi(&brand(top_bar), mode), foreground_ansi(&brand(right_leg), mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive_components::test_support::plain;
    use crate::theme::tests::global_state;

    #[test]
    fn draws_the_gear_logo_in_half_blocks() {
        let _theme = global_state();
        let lines = plain(&pi_logo_lines());
        assert_eq!(lines, [" ▄▄██▄▄ ", "▄█▀▀███▄", "▀██▀███▀", " ▀▀██▀▀ "]);
    }

    #[test]
    fn glyph_offset_points_at_the_upstream_pi_glyph() {
        let (column, line) = PI_GLYPH_OFFSET;
        let glyph: Vec<&str> = PIXELS[line * 2..line * 2 + 4].iter().map(|row| &row[column..column + 4]).collect();
        assert_eq!(glyph, ["ttti", "liti", "llir", "liir"]);
    }
}
