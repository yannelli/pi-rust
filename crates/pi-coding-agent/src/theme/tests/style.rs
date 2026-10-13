//! Theme styles, token fallbacks, appearance detection, settings, and export colors (upstream
//! `theme-style`, `scrollbar-theme`, `theme-detection`, `theme-export`, and `theme-picker` tests).

use std::sync::Arc;

use pi_tui::colors::{Color, TerminalColorMode, TextStyle, color_to_hex, okhsl_color, style_text};
use pi_tui::env::MapEnv;
use pi_tui::terminal_colors::{RgbColor, TerminalColors};
use serde_json::json;

use super::*;

#[test]
fn renders_theme_tokens_the_same_as_the_generic_text_styler() {
    let _state = global_state();
    let theme = load_edited("dark", |_| {});
    let style = ThemeStyle {
        fg: Some(StyleFg::Token(ThemeColor::Success)),
        bg: Some(StyleBg::Token(ThemeBg::ToolSuccessBg)),
        attributes: pi_tui::colors::TextAttributes { bold: true, ..Default::default() },
    };
    let colors = theme.colors();
    let generic = TextStyle {
        fg: Some(*colors.get(ThemeColor::Success)),
        bg: Some(*colors.get(ThemeBg::ToolSuccessBg)),
        bold: true,
        ..TextStyle::default()
    };
    assert_eq!(theme.style("Ready", &style), style_text("Ready", &generic, TerminalColorMode::Truecolor));
}

#[test]
fn rejects_unknown_tokens_and_tokens_in_the_wrong_slot() {
    let theme = load_edited("dark", |_| {});
    let attributes = Default::default();
    let unknown = theme.style_named("x", Some("notAToken"), None, attributes).unwrap_err();
    assert_eq!(unknown.0, "Unknown theme color: notAToken");
    let wrong_slot = theme.style_named("x", Some("userMessageBg"), None, attributes).unwrap_err();
    assert_eq!(wrong_slot.0, "Unknown theme color: userMessageBg");
    assert!(theme.fg_named("accent", "x").is_ok());
    assert!(theme.bg_named("accent", "x").is_err());
}

#[test]
fn loads_oklch_and_okhsl_values_including_through_variables() {
    let theme = load_edited("dark", |json| json["colors"]["accent"] = json!("oklch(62% 0.1 200)"));
    assert_eq!(theme.color(ThemeColor::Accent), Color::Oklch { l: 0.62, c: 0.1, h: 200.0 });
    let theme = load_edited("dark", |json| {
        json["vars"]["brand"] = json!("okhsl(250 60% 55%)");
        json["colors"]["accent"] = json!("brand");
        json["colors"]["error"] = json!("okhsl(20 90% 60%)");
    });
    let hex = |h, s, l| color_to_hex(&okhsl_color(h, s, l).expect("okhsl"));
    assert_eq!(color_to_hex(&theme.color(ThemeColor::Accent)), hex(250.0, 0.6, 0.55));
    assert_eq!(color_to_hex(&theme.color(ThemeColor::Error)), hex(20.0, 0.9, 0.6));
}

#[test]
fn detects_the_appearance_unless_it_is_declared() {
    let _state = global_state();
    assert_eq!(load_edited("dark", |_| {}).appearance(), TerminalTheme::Dark);
    assert_eq!(load_edited("light", |_| {}).appearance(), TerminalTheme::Light);
    for (base, expected) in [("dark", TerminalTheme::Dark), ("light", TerminalTheme::Light)] {
        let theme = load_edited(base, |json| {
            json.as_object_mut().expect("object").remove("appearance");
        });
        assert_eq!(theme.appearance(), expected);
    }
    assert_eq!(load_edited("dark", |json| json["appearance"] = json!("light")).appearance(), TerminalTheme::Light);
    // Palette colors 0-15 follow the terminal palette, so such themes follow the terminal background.
    let palette_only = load_edited("dark", |json| {
        json.as_object_mut().expect("object").remove("appearance");
        for (key, value) in json["colors"].as_object_mut().expect("colors") {
            *value = json!(if key.ends_with("Bg") { 0 } else { 7 });
        }
    });
    assert_eq!(palette_only.appearance(), TerminalTheme::Dark);
    set_terminal_colors(TerminalColors { background: Some(RgbColor::new(250, 250, 250)), ..Default::default() });
    assert_eq!(palette_only.appearance(), TerminalTheme::Light);
}

#[test]
fn renders_empty_tokens_as_terminal_defaults_and_reports_concrete_colors() {
    let _state = global_state();
    let theme = load_edited("dark", |json| {
        json["colors"]["text"] = json!("");
        json["colors"]["userMessageBg"] = json!("");
    });
    assert_eq!(theme.fg(ThemeColor::Text, "x"), "\x1b[39mx\x1b[39m");
    assert_eq!(theme.bg(ThemeBg::UserMessageBg, "x"), "\x1b[49mx\x1b[49m");
    assert_eq!(color_to_hex(&theme.color(ThemeColor::Text)), "#e5e5e7");
    assert_eq!(color_to_hex(&theme.color(ThemeBg::UserMessageBg)), "#000000");
    set_terminal_colors(TerminalColors {
        foreground: Some(RgbColor::new(200, 210, 220)),
        background: Some(RgbColor::new(10, 20, 30)),
        palette: None,
    });
    assert_eq!(color_to_hex(&theme.color(ThemeColor::Text)), "#c8d2dc");
    assert_eq!(color_to_hex(&theme.color(ThemeBg::UserMessageBg)), "#0a141e");
}

#[test]
fn optional_tokens_fall_back_and_explicit_values_win() {
    for (token, fallback) in
        [(ThemeColor::ScrollbarTrack, ThemeColor::Muted), (ThemeColor::ScrollbarThumb, ThemeColor::Text)]
    {
        let theme = load_edited("dark", |json| {
            json["colors"].as_object_mut().expect("colors").remove(token.name());
        });
        assert_eq!(theme.get_fg_ansi(token), theme.get_fg_ansi(fallback));
    }
    let theme = load_edited("dark", |json| {
        json["colors"]["scrollbarTrack"] = json!("#654321");
        json["colors"]["scrollbarThumb"] = json!("#123456");
    });
    assert_eq!(theme.get_fg_ansi(ThemeColor::ScrollbarTrack), "\x1b[38;2;101;67;33m");
    assert_eq!(theme.get_fg_ansi(ThemeColor::ScrollbarThumb), "\x1b[38;2;18;52;86m");
    let theme = load_edited("dark", |json| {
        let colors = json["colors"].as_object_mut().expect("colors");
        colors.remove("searchMatchBg");
        colors.remove("searchMatchText");
    });
    assert_eq!(theme.get_bg_ansi(ThemeBg::SearchMatchBg), theme.get_bg_ansi(ThemeBg::SelectedBg));
    assert_eq!(theme.get_fg_ansi(ThemeColor::SearchMatchText), theme.get_fg_ansi(ThemeColor::Text));
    let theme = load_edited("dark", |json| {
        json["colors"]["searchMatchBg"] = json!("#112233");
        json["colors"]["searchMatchText"] = json!("#223344");
    });
    assert_eq!(theme.get_bg_ansi(ThemeBg::SearchMatchBg), "\x1b[48;2;17;34;51m");
    assert_eq!(theme.get_fg_ansi(ThemeColor::SearchMatchText), "\x1b[38;2;34;51;68m");
}

#[test]
fn color_fg_bg_classifies_the_last_field_like_vim() {
    let detect = |value: &str| detect_color_fg_bg_theme(&MapEnv::new([("COLORFGBG", value)]));
    assert_eq!(detect("15;0"), Some(TerminalTheme::Dark));
    assert_eq!(detect("0;7;15"), Some(TerminalTheme::Light));
    assert_eq!(detect("12;8"), Some(TerminalTheme::Dark));
    assert_eq!(detect("15;default"), None);
    assert_eq!(detect_color_fg_bg_theme(&MapEnv::default()), None);
}

#[test]
fn terminal_theme_prefers_background_then_report_then_color_fg_bg_then_dark() {
    let env = MapEnv::new([("COLORFGBG", "0;15")]);
    let background = |r, g, b| TerminalColors { background: Some(RgbColor::new(r, g, b)), ..Default::default() };
    let none = TerminalColors::default();
    assert_eq!(detect_terminal_theme(&background(8, 8, 8), Some(TerminalTheme::Light), &env), TerminalTheme::Dark);
    assert_eq!(detect_terminal_theme(&none, Some(TerminalTheme::Dark), &env), TerminalTheme::Dark);
    assert_eq!(detect_terminal_theme(&none, None, &env), TerminalTheme::Light);
    assert_eq!(detect_terminal_theme(&none, None, &MapEnv::default()), TerminalTheme::Dark);
    let empty = MapEnv::default();
    assert_eq!(detect_terminal_theme(&background(118, 118, 118), None, &empty), TerminalTheme::Light);
    let white = Some(RgbColor::new(255, 255, 255));
    let with_fg = |r, g, b| TerminalColors { foreground: white, ..background(r, g, b) };
    assert_eq!(detect_terminal_theme(&with_fg(118, 118, 118), None, &empty), TerminalTheme::Dark);
    // White text cannot reach 4.5:1 on mid-gray.
    assert_eq!(detect_terminal_theme(&with_fg(128, 128, 128), None, &empty), TerminalTheme::Light);
}

#[test]
fn theme_color_mode_follows_terminal_capabilities() {
    let _state = global_state();
    let _caps = pi_tui::env::global_state_lock();
    let caps =
        |true_color| pi_tui::terminal_image::TerminalCapabilities { images: None, true_color, hyperlinks: false };
    pi_tui::terminal_image::set_capabilities(caps(false));
    let theme = get_theme_by_name("dark").expect("dark");
    assert_eq!(theme.get_color_mode(), TerminalColorMode::Color256);
    assert!(theme.get_fg_ansi(ThemeColor::Accent).starts_with("\x1b[38;5;"));
    pi_tui::terminal_image::set_capabilities(caps(true));
    let theme = get_theme_by_name("dark").expect("dark");
    assert_eq!(theme.get_color_mode(), TerminalColorMode::Truecolor);
    assert!(theme.get_fg_ansi(ThemeColor::Accent).starts_with("\x1b[38;2;"));
    pi_tui::terminal_image::reset_capabilities_cache();
}

#[test]
fn parses_and_resolves_automatic_theme_settings() {
    let auto = parse_auto_theme_setting(Some("light/dark")).expect("pair");
    assert_eq!((auto.light_theme.as_str(), auto.dark_theme.as_str()), ("light", "dark"));
    assert_eq!(resolve_theme_setting(Some("dark"), TerminalTheme::Light).as_deref(), Some("dark"));
    assert_eq!(resolve_theme_setting(Some("light/dark"), TerminalTheme::Light).as_deref(), Some("light"));
    assert_eq!(resolve_theme_setting(Some("light/dark"), TerminalTheme::Dark).as_deref(), Some("dark"));
    assert_eq!(resolve_theme_setting(Some("light/dark/extra"), TerminalTheme::Dark), None);
    assert_eq!(parse_auto_theme_setting(Some(" / dark")), None);
}

fn custom_dir_with(themes: &[serde_json::Value]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for theme in themes {
        write_theme(dir.path(), theme);
    }
    set_custom_themes_dir(Some(dir.path().to_path_buf()));
    dir
}

#[test]
fn export_colors_resolve_variables_okhsl_and_palette_indices() {
    let _state = global_state();
    let mut vars_theme = builtin_json("dark");
    vars_theme["name"] = json!("custom-export-vars");
    for (key, value) in
        [("pageBgVar", "#112233"), ("pageBgAlias", "pageBgVar"), ("infoBgVar", "#445566"), ("cardBgVar", "#223344")]
    {
        vars_theme["vars"][key] = json!(value);
    }
    vars_theme["export"] = json!({ "pageBg": "pageBgAlias", "cardBg": "cardBgVar", "infoBg": "infoBgVar" });
    let mut okhsl_theme = builtin_json("dark");
    okhsl_theme["name"] = json!("custom-export-okhsl");
    okhsl_theme["vars"] = json!({ "card": "okhsl(250 20% 20%)" });
    okhsl_theme["export"] = json!({ "pageBg": "okhsl(250 20% 15%)", "cardBg": "card", "infoBg": "oklch(30% 0.05 80)" });
    let mut recursive = builtin_json("dark");
    recursive["name"] = json!("custom-export-recursive");
    for (key, value) in
        [("deepPageBg", json!("#abcdef")), ("pageBgAlias", json!("deepPageBg")), ("cardBgAnsi", json!(24))]
    {
        recursive["vars"][key] = value;
    }
    recursive["export"] = json!({ "pageBg": "pageBgAlias", "cardBg": "cardBgAnsi", "infoBg": "" });
    let _dir = custom_dir_with(&[vars_theme, okhsl_theme, recursive]);

    let colors = |page: &str, card: &str, info: Option<&str>| ThemeExportColors {
        page_bg: Some(page.to_string()),
        card_bg: Some(card.to_string()),
        info_bg: info.map(String::from),
    };
    assert_eq!(get_theme_export_colors(Some("custom-export-vars")), colors("#112233", "#223344", Some("#445566")));
    let hex = |h, s, l| color_to_hex(&okhsl_color(h, s, l).expect("okhsl"));
    let expected = colors(&hex(250.0, 0.2, 0.15), &hex(250.0, 0.2, 0.2), Some("oklch(30% 0.05 80)"));
    assert_eq!(get_theme_export_colors(Some("custom-export-okhsl")), expected);
    assert_eq!(get_theme_export_colors(Some("custom-export-recursive")), colors("#abcdef", "#005f87", None));
}

#[test]
fn theme_picker_uses_theme_names_instead_of_file_names() {
    let _state = global_state();
    let dir = tempfile::tempdir().expect("temp dir");
    let mut theme = builtin_json("dark");
    theme["name"] = json!("bar");
    let path = dir.path().join("foo.json");
    std::fs::write(&path, theme.to_string()).expect("write");
    set_custom_themes_dir(Some(dir.path().to_path_buf()));
    let names = get_available_themes();
    assert!(names.contains(&"bar".to_string()));
    assert!(!names.contains(&"foo".to_string()));
    assert!(get_all_themes().contains(&ThemeInfo { name: "bar".into(), path: Some(path) }));
}

#[test]
fn registered_themes_are_listed_and_names_with_slashes_are_rejected() {
    let _state = global_state();
    let theme = Arc::new(load_edited("light", |json| json["name"] = json!("zeta")));
    set_registered_themes(vec![theme.clone()]).expect("register");
    assert_eq!(get_available_themes(), vec!["system", "dark", "light", "zeta"]);
    assert!(Arc::ptr_eq(&get_theme("zeta").expect("registered"), &theme));
    let invalid = Theme::new(
        TokenMap::from_fn(|_| Some(pi_tui::colors::ColorValue::Text(String::new()))),
        TerminalColorMode::Truecolor,
        ThemeOptions { name: Some("a/b".into()), ..Default::default() },
    )
    .expect("theme");
    let error = set_registered_themes(vec![Arc::new(invalid)]).unwrap_err();
    assert!(error.0.starts_with("Invalid theme name \"a/b\""));
}

#[test]
fn set_theme_falls_back_to_system_on_errors() {
    let _state = global_state();
    assert!(set_theme("light", false).is_ok());
    assert_eq!(theme().name(), Some("light"));
    let error = set_theme("missing-theme", false).unwrap_err();
    assert_eq!(error.0, "Theme not found: missing-theme");
    assert_eq!(theme().name(), Some("system"));
    assert_eq!(current_theme_name().as_deref(), Some("system"));
}
