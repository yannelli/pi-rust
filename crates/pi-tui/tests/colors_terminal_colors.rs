//! Port of the parser cases in upstream `test/terminal-colors.test.ts`.

use pi_tui::terminal_colors::{
    OscColorResponse, OscColorTarget, RgbColor, TerminalColorScheme, parse_osc_color_response,
    parse_terminal_color_scheme_report,
};

#[test]
fn parses_color_scheme_reports() {
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;1n"), Some(TerminalColorScheme::Dark));
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;2n"), Some(TerminalColorScheme::Light));
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
        Some(TerminalColorScheme::Dark)
    );
    assert_eq!(
        parse_terminal_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
        Some(TerminalColorScheme::Light)
    );
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?997;3n"), None);
    assert_eq!(parse_terminal_color_scheme_report("\x1b[?996n"), None);
    assert_eq!(parse_terminal_color_scheme_report("x\x1b[?997;1n"), None);
}

#[test]
fn parses_osc_10_11_and_4_replies() {
    assert_eq!(
        parse_osc_color_response("\x1b]10;rgb:ffff/ffff/ffff\x07"),
        Some(OscColorResponse { target: OscColorTarget::Foreground, rgb: Some(RgbColor { r: 255, g: 255, b: 255 }) })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;13;#ff0080\x1b\\"),
        Some(OscColorResponse { target: OscColorTarget::Palette(13), rgb: Some(RgbColor { r: 255, g: 0, b: 128 }) })
    );
    assert_eq!(
        parse_osc_color_response("\x1b]4;1;bogus\x07"),
        Some(OscColorResponse { target: OscColorTarget::Palette(1), rgb: None })
    );
    assert_eq!(parse_osc_color_response("\x1b]12;#ffffff\x07"), None);
}

#[test]
fn parses_other_color_value_forms() {
    let parse = |data: &str| parse_osc_color_response(data).and_then(|response| response.rgb);
    assert_eq!(parse("\x1b]11;rgb:0000/0000/0000\x1b\\"), Some(RgbColor { r: 0, g: 0, b: 0 }));
    assert_eq!(parse("\x1b]11;rgba:80/40/20/ff\x07"), Some(RgbColor { r: 128, g: 64, b: 32 }));
    assert_eq!(parse("\x1b]11;RGB:f/8/0\x07"), Some(RgbColor { r: 255, g: 136, b: 0 }));
    assert_eq!(parse("\x1b]11;#ffff80000000\x07"), Some(RgbColor { r: 255, g: 128, b: 0 }));
    assert_eq!(parse("\x1b]11;#fff\x07"), None);
    assert_eq!(parse("\x1b]11;rgb:ff/ff\x07"), None);
    assert_eq!(
        parse_osc_color_response("\x1b]11;#000000\x07").map(|response| response.target),
        Some(OscColorTarget::Background)
    );
    assert_eq!(parse_osc_color_response("\x1b]4;300;#000000\x07"), None);
}
