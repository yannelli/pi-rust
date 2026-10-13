//! Built-in themes compared with upstream output: resolved colors, escape sequences, export colors.

use pi_tui::colors::{TerminalColorMode, color_to_hex};
use serde_json::Value;

use super::*;

const FIXTURE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/theme/builtin-themes.json"));

fn builtin_theme(name: &str, mode: TerminalColorMode) -> Theme {
    let json = crate::theme::schema::theme_json_from_value(&builtin_json(name));
    create_theme(&json, Some(mode), None).expect("built-in theme builds")
}

#[test]
fn builtin_themes_validate_against_the_schema() {
    for name in BUILTIN_THEME_NAMES {
        validate_theme_json(name, &builtin_json(name)).expect("built-in theme is valid");
    }
}

#[test]
fn builtin_themes_match_upstream_colors_and_escape_sequences() {
    let _state = global_state();
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    for name in BUILTIN_THEME_NAMES {
        let expected = &fixture[name];
        for (mode, key) in [(TerminalColorMode::Truecolor, "truecolor"), (TerminalColorMode::Color256, "256color")] {
            let theme = builtin_theme(name, mode);
            assert_eq!(theme.appearance().as_str(), expected["appearance"], "{name}");
            let colors = theme.colors();
            for (token, color) in colors.iter() {
                assert_eq!(color_to_hex(color), expected["colors"][token.name()], "{name} {token}");
                let ansi = match ThemeColor::try_from(token) {
                    Ok(fg) => theme.get_fg_ansi(fg),
                    Err(_) => theme.get_bg_ansi(ThemeBg::try_from(token).expect("bg token")).to_string(),
                };
                assert_eq!(ansi, expected[key][token.name()], "{name} {key} {token}");
            }
        }
        let export = get_theme_export_colors(Some(name));
        let field = |value: Option<String>| value.map(Value::String).unwrap_or(Value::Null);
        assert_eq!(field(export.page_bg), expected["export"]["pageBg"], "{name}");
        assert_eq!(field(export.card_bg), expected["export"]["cardBg"], "{name}");
        assert_eq!(field(export.info_bg), expected["export"]["infoBg"], "{name}");
    }
}

#[test]
fn export_theme_carries_resolved_colors_and_export_backgrounds() {
    let _state = global_state();
    let theme = export_theme(Some("light")).expect("export theme");
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    assert_eq!(theme.colors.len(), TOKEN_COUNT);
    assert_eq!(theme.colors[0].0, "accent");
    for (token, hex) in &theme.colors {
        assert_eq!(fixture["light"]["colors"][token], hex.as_str(), "{token}");
    }
    assert_eq!(theme.page_bg.as_deref(), fixture["light"]["export"]["pageBg"].as_str());
    assert!(theme.css_vars().contains("--exportPageBg: #efeeee;"));
    assert!(is_light_theme(Some("light")).expect("light"));
    assert!(!is_light_theme(Some("dark")).expect("dark"));
}
