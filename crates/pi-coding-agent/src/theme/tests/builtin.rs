//! Built-in themes compared with upstream output: resolved colors, escape sequences, export colors.

use pi_tui::colors::{TerminalColorMode, color_to_hex, color_to_rgb, parse_color};
use serde_json::Value;

use super::*;

const FIXTURE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/theme/builtin-themes.json"));
const ADDITIONAL_THEMES: [&str; 4] = ["ghostty", "darcula-islands", "nord", "catppuccin"];

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
    for name in ["dark", "light"] {
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

#[test]
fn additional_builtin_themes_are_discoverable_and_load_without_files() {
    let _state = global_state();
    let dir = tempfile::tempdir().expect("temp dir");
    set_custom_themes_dir(Some(dir.path().to_path_buf()));
    let infos = get_available_themes_with_paths();
    for name in ADDITIONAL_THEMES {
        assert!(BUILTIN_THEME_NAMES.contains(&name), "{name}");
        assert_eq!(infos.iter().filter(|info| info.name == name).count(), 1, "{name}");
        assert!(infos.contains(&ThemeInfo { name: name.into(), path: None }), "{name}");
        assert_eq!(get_theme_by_name(name).expect("built-in theme").name(), Some(name));
        set_theme(name, false).expect("select built-in theme");
        assert_eq!(current_theme_name().as_deref(), Some(name));
        assert_eq!(theme().name(), Some(name));
        for (mode, fg, bg) in [
            (TerminalColorMode::Truecolor, "\x1b[38;2;", "\x1b[48;2;"),
            (TerminalColorMode::Color256, "\x1b[38;5;", "\x1b[48;5;"),
        ] {
            let loaded = crate::theme::registry::load_theme(name, Some(mode)).expect("load built-in theme");
            assert_eq!(loaded.appearance(), TerminalTheme::Dark, "{name}");
            assert_eq!(loaded.get_color_mode(), mode, "{name}");
            assert_eq!(loaded.source_path(), None, "{name}");
            for token in ThemeColor::ALL {
                assert!(loaded.get_fg_ansi(*token).starts_with(fg), "{name} {token}");
            }
            for token in ThemeBg::ALL {
                assert!(loaded.get_bg_ansi(*token).starts_with(bg), "{name} {token}");
            }
        }
        let exported = export_theme(Some(name)).expect("export built-in theme");
        assert_eq!(exported.colors.len(), TOKEN_COUNT, "{name}");
        assert!(exported.page_bg.is_some(), "{name}");
        assert!(exported.card_bg.is_some(), "{name}");
        assert!(exported.info_bg.is_some(), "{name}");
    }
}

#[test]
fn additional_builtin_themes_keep_body_text_readable_on_their_surfaces() {
    let _state = global_state();
    for name in ADDITIONAL_THEMES {
        let theme = builtin_theme(name, TerminalColorMode::Truecolor);
        let contrast = |fg: ThemeColor, background| {
            let ratio = wcag_contrast(color_to_rgb(&theme.color(fg)), background);
            assert!(ratio >= 4.5, "{name} {fg}: contrast {ratio} on {background:?}");
        };
        let exported = get_theme_export_colors(Some(name));
        for surface in [exported.page_bg, exported.card_bg, exported.info_bg] {
            let surface = color_to_rgb(&parse_color(&surface.expect("export surface")).expect("color"));
            contrast(ThemeColor::Text, surface);
            contrast(ThemeColor::Muted, surface);
        }
        for (fg, bg) in [
            (ThemeColor::Text, ThemeBg::SelectedBg),
            (ThemeColor::SearchMatchText, ThemeBg::SearchMatchBg),
            (ThemeColor::UserMessageText, ThemeBg::UserMessageBg),
            (ThemeColor::CustomMessageText, ThemeBg::CustomMessageBg),
            (ThemeColor::CustomMessageLabel, ThemeBg::CustomMessageBg),
        ] {
            contrast(fg, color_to_rgb(&theme.color(bg)));
        }
        for surface in [ThemeBg::ToolPendingBg, ThemeBg::ToolSuccessBg, ThemeBg::ToolErrorBg] {
            let background = color_to_rgb(&theme.color(surface));
            for text in [
                ThemeColor::ToolTitle,
                ThemeColor::ToolOutput,
                ThemeColor::ToolDiffAdded,
                ThemeColor::ToolDiffRemoved,
                ThemeColor::ToolDiffContext,
                ThemeColor::SyntaxComment,
                ThemeColor::SyntaxKeyword,
                ThemeColor::SyntaxFunction,
                ThemeColor::SyntaxVariable,
                ThemeColor::SyntaxString,
                ThemeColor::SyntaxNumber,
                ThemeColor::SyntaxType,
                ThemeColor::SyntaxOperator,
                ThemeColor::SyntaxPunctuation,
            ] {
                contrast(text, background);
            }
        }
    }
}
