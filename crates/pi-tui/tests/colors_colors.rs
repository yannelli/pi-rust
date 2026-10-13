//! Port of upstream `test/colors.test.ts`, plus checks for the Rust-specific API.

use pi_tui::colors::{
    Color, ColorMixSpace, ColorValue, TerminalColorMode, TextAttributes, TextStyle, color_to_hex, color_to_okhsl,
    color_to_oklch, color_to_rgb, foreground_ansi, indexed_color, indexed_color_from_number, mix_colors, okhsl_color,
    oklch_color, parse_color, parse_color_value, rgb_color, style_text, style_text_with_ansi,
};
use pi_tui::oklab::oklab_to_okhsl_lightness;
use pi_tui::terminal_colors::RgbColor;
use regex::Regex;

fn rgb(r: u8, g: u8, b: u8) -> RgbColor {
    RgbColor { r, g, b }
}

#[test]
fn parses_hex_and_oklch_colors_and_rejects_everything_else() {
    assert_eq!(parse_color("#abc").unwrap(), Color::Rgb { r: 170.0, g: 187.0, b: 204.0 });
    assert_eq!(parse_color("oklch(62% 0.1 200)").unwrap(), Color::Oklch { l: 0.62, c: 0.1, h: 200.0 });
    assert!(parse_color("").unwrap_err().to_string().contains("Invalid color value"));
    assert!(parse_color("red").unwrap_err().to_string().contains("Invalid color value"));
}

#[test]
fn gamut_maps_oklch_to_srgb_including_the_lightness_limits() {
    assert_eq!(color_to_rgb(&oklch_color(0.627955, 0.257683, 29.2339).unwrap()), rgb(255, 0, 0));
    assert_eq!(color_to_rgb(&oklch_color(1.0, 0.3, 150.0).unwrap()), rgb(255, 255, 255));
    assert_eq!(color_to_rgb(&oklch_color(0.0, 0.3, 150.0).unwrap()), rgb(0, 0, 0));
}

#[test]
fn parses_okhsl_colors_and_round_trips_them() {
    // Full saturation at the red cusp is pure sRGB red.
    assert_eq!(parse_color("okhsl(29.23 100% 56.8%)").unwrap(), rgb_color(255.0, 0.0, 0.0).unwrap());
    assert_eq!(parse_color("OKHSL(250deg 60% 55%)").unwrap(), okhsl_color(250.0, 0.6, 0.55).unwrap());
    assert!(parse_color("okhsl(250 160% 55%)").unwrap_err().to_string().contains("s must be between 0 and 1"));
    for hex in ["#4f8eb3", "#20242a", "#f8f9fa"] {
        let okhsl = color_to_okhsl(&parse_color(hex).unwrap());
        assert_eq!(color_to_hex(&okhsl_color(okhsl.h, okhsl.s, okhsl.l).unwrap()), hex);
    }
}

#[test]
fn styles_text_and_closes_sequences_in_reverse_order() {
    let style = TextStyle {
        fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()),
        bg: Some(indexed_color(9).unwrap()),
        bold: true,
        italic: true,
        ..TextStyle::default()
    };
    assert_eq!(
        style_text("Ready", &style, TerminalColorMode::Truecolor),
        "\x1b[38;2;18;52;86m\x1b[48;5;9m\x1b[1m\x1b[3mReady\x1b[23m\x1b[22m\x1b[49m\x1b[39m"
    );
    let fg_only = TextStyle { fg: Some(rgb_color(18.0, 52.0, 86.0).unwrap()), ..TextStyle::default() };
    let pattern = Regex::new(r"^\x1b\[38;5;\d+mReady\x1b\[39m$").unwrap();
    assert!(pattern.is_match(&style_text("Ready", &fg_only, TerminalColorMode::Color256)));
}

#[test]
fn rejects_out_of_range_values_with_upstream_messages() {
    assert_eq!(indexed_color(256).unwrap_err().0, "ANSI color index must be an integer from 0 to 255: 256");
    assert_eq!(indexed_color_from_number(3.5).unwrap_err().0, "ANSI color index must be an integer from 0 to 255: 3.5");
    assert_eq!(rgb_color(0.0, 256.0, 0.0).unwrap_err().0, "g must be between 0 and 255: 256");
    assert_eq!(rgb_color(f64::NAN, 0.0, 0.0).unwrap_err().0, "r must be finite");
    assert_eq!(oklch_color(1.5, 0.0, 0.0).unwrap_err().0, "l must be between 0 and 1: 1.5");
    assert_eq!(oklch_color(0.5, -0.1, 0.0).unwrap_err().0, "c must not be negative: -0.1");
    let black = Color::Indexed(0);
    assert_eq!(
        mix_colors(&black, &black, 2.0, ColorMixSpace::Oklch).unwrap_err().0,
        "amount must be between 0 and 1: 2"
    );
    assert_eq!(oklch_color(0.5, 0.1, -30.0).unwrap(), Color::Oklch { l: 0.5, c: 0.1, h: 330.0 });
}

#[test]
fn parses_theme_values_that_are_strings_or_numbers() {
    let values: Vec<ColorValue> = serde_json::from_str(r##"["#ff0000", 9, "oklch(0.5 0 0)"]"##).unwrap();
    assert_eq!(parse_color_value(&values[0]).unwrap(), Color::Rgb { r: 255.0, g: 0.0, b: 0.0 });
    assert_eq!(parse_color_value(&values[1]).unwrap(), Color::Indexed(9));
    assert_eq!(parse_color_value(&values[2]).unwrap(), Color::Oklch { l: 0.5, c: 0.0, h: 0.0 });
    assert!(parse_color_value(&ColorValue::Number(300.0)).is_err());
    assert_eq!(parse_color("oklch(+.5 1.e-1 2E2deg)").unwrap(), Color::Oklch { l: 0.5, c: 0.1, h: 200.0 });
}

#[test]
fn converts_indexed_colors_and_mixes() {
    assert_eq!(color_to_hex(&Color::Indexed(9)), "#ff0000");
    assert_eq!(color_to_hex(&Color::Indexed(16 + 36 * 5 + 6 * 2 + 1)), "#ff875f");
    assert_eq!(color_to_hex(&Color::Indexed(232)), "#080808");
    let black = Color::Indexed(0);
    let white = Color::Indexed(15);
    let gray = mix_colors(&black, &white, 0.5, ColorMixSpace::Srgb).unwrap();
    assert_eq!(gray, Color::Rgb { r: 127.5, g: 127.5, b: 127.5 });
    assert_eq!(color_to_rgb(&gray), rgb(128, 128, 128));
    let perceptual = mix_colors(&black, &white, 0.5, ColorMixSpace::Oklch).unwrap();
    assert!((color_to_oklch(&perceptual).l - 0.5).abs() < 1e-6);
    assert_eq!(
        foreground_ansi(&Color::Rgb { r: 128.0, g: 128.0, b: 128.0 }, TerminalColorMode::Color256),
        "\x1b[38;5;244m"
    );
    assert!((oklab_to_okhsl_lightness(1.0) - 1.0).abs() < 1e-6);
}

#[test]
fn styles_with_precomputed_sequences() {
    let attributes = TextAttributes { dim: true, underline: true, strikethrough: true, ..TextAttributes::default() };
    assert_eq!(
        style_text_with_ansi("x", Some(""), Some("\x1b[48;5;1m"), &attributes),
        "\x1b[48;5;1m\x1b[2m\x1b[4m\x1b[9mx\x1b[29m\x1b[24m\x1b[22m\x1b[49m"
    );
    assert_eq!(
        style_text_with_ansi("x", None, None, &TextAttributes { inverse: true, ..TextAttributes::default() }),
        "\x1b[7mx\x1b[27m"
    );
}
