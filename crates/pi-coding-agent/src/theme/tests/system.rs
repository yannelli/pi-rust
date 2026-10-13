//! The generated `system` theme (upstream `system-theme.test.ts`) and fixtures of upstream output.

use pi_tui::colors::{ColorValue, color_to_oklch, color_to_rgb, parse_color};
use pi_tui::terminal_colors::{RgbColor, TerminalColors};
use serde_json::Value;

use super::*;

const FIXTURE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/theme/system-theme.json"));

fn rgb(hex: &str) -> RgbColor {
    color_to_rgb(&parse_color(hex).expect("hex color"))
}

fn lightness(color: RgbColor) -> f64 {
    color_to_oklch(&pi_tui::colors::Color::Rgb { r: color.r.into(), g: color.g.into(), b: color.b.into() }).l
}

fn input_from(value: &Value) -> SystemThemeInput {
    let color = |key: &str| value[key].as_str().map(rgb);
    SystemThemeInput {
        background: color("background"),
        foreground: color("foreground"),
        palette: value["palette"].as_array().map(|palette| palette.iter().filter_map(Value::as_str).map(rgb).collect()),
        saturation: value["saturation"].as_f64(),
        appearance_hint: match value["appearanceHint"].as_str() {
            Some("light") => Some(TerminalTheme::Light),
            Some("dark") => Some(TerminalTheme::Dark),
            _ => None,
        },
    }
}

fn to_json(value: &ColorValue) -> Value {
    match value {
        ColorValue::Text(text) => Value::String(text.clone()),
        ColorValue::Number(number) => Value::from(*number as i64),
    }
}

#[test]
fn generated_colors_match_upstream_for_every_terminal_fixture() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    for (name, case) in fixture.as_object().expect("object") {
        let generated = generate_system_theme_colors(&input_from(&case["input"]));
        for (token, value) in generated.colors.iter() {
            assert_eq!(to_json(value), case["colors"][token.name()], "{name} {token}");
        }
        let mut dim: Vec<&str> = generated.dim.iter().map(|color| color.name()).collect();
        let mut expected: Vec<&str> = case["dim"].as_array().expect("dim").iter().filter_map(Value::as_str).collect();
        dim.sort_unstable();
        expected.sort_unstable();
        assert_eq!(dim, expected, "{name}");
        assert_eq!(generated.appearance.map(|a| a.as_str()), case["appearance"].as_str(), "{name}");
    }
}

fn dracula() -> SystemThemeInput {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    input_from(&fixture["dracula"]["input"])
}

fn terminals() -> Vec<(&'static str, SystemThemeInput)> {
    let input = |background: &str, foreground: Option<&str>| SystemThemeInput {
        background: Some(rgb(background)),
        foreground: foreground.map(rgb),
        ..SystemThemeInput::default()
    };
    vec![
        ("dracula", dracula()),
        ("solarizedLight", input("#fdf6e3", Some("#657b83"))),
        ("backgroundOnly", input("#1e1e1e", None)),
        ("midGray", input("#808080", Some("#ffffff"))),
    ]
}

const PANELS: [ThemeToken; 5] = [
    ThemeToken::UserMessageBg,
    ThemeToken::ToolPendingBg,
    ThemeToken::ToolSuccessBg,
    ThemeToken::ToolErrorBg,
    ThemeToken::SelectedBg,
];

fn resolved(input: &SystemThemeInput, token: ThemeToken) -> RgbColor {
    match generate_system_theme_colors(input).colors.get(token) {
        ColorValue::Text(text) if text.is_empty() => {
            if PANELS.contains(&token) {
                input.background.expect("bg")
            } else {
                input.foreground.expect("fg")
            }
        }
        ColorValue::Text(hex) => rgb(hex),
        ColorValue::Number(_) => panic!("unexpected palette index"),
    }
}

#[test]
fn keeps_body_text_readable_on_the_background_and_panels() {
    for (name, input) in terminals() {
        let text = resolved(&input, ThemeToken::Text);
        for surface in [input.background.expect("bg"), resolved(&input, ThemeToken::SelectedBg)] {
            assert!(wcag_contrast(text, surface) >= 4.5, "{name}");
        }
        let title = resolved(&input, ThemeToken::ToolTitle);
        assert!(wcag_contrast(title, resolved(&input, ThemeToken::ToolErrorBg)) >= 4.5, "{name}");
    }
}

#[test]
fn orders_foreground_roles_by_contrast_and_keeps_panels_close_to_the_background() {
    for (name, input) in terminals() {
        let background = lightness(input.background.expect("bg"));
        let offset = |token| lightness(resolved(&input, token)) - background;
        if name != "midGray" {
            assert!(offset(ThemeToken::Text).abs() > offset(ThemeToken::Muted).abs(), "{name}");
            assert!(offset(ThemeToken::Muted).abs() > offset(ThemeToken::Dim).abs(), "{name}");
        }
        let lighter = generate_system_theme_colors(&input).appearance == Some(TerminalTheme::Dark);
        for panel in PANELS {
            assert!(wcag_contrast(resolved(&input, panel), input.background.expect("bg")) < 2.0, "{name} {panel}");
            assert_eq!(offset(panel) > 0.0, lighter, "{name} {panel}");
        }
    }
}

#[test]
fn uses_the_terminal_foreground_and_palette_hues() {
    let dracula = dracula();
    assert_eq!(generate_system_theme_colors(&dracula).colors.get(ThemeToken::Text), &ColorValue::Text(String::new()));
    let solarized = &terminals()[1].1;
    assert_ne!(generate_system_theme_colors(solarized).colors.get(ThemeToken::Text), &ColorValue::Text(String::new()));
    let hue = |color: RgbColor| {
        color_to_oklch(&pi_tui::colors::Color::Rgb { r: color.r.into(), g: color.g.into(), b: color.b.into() }).h
    };
    let palette = dracula.palette.clone().expect("palette");
    assert!((hue(resolved(&dracula, ThemeToken::Error)) - hue(palette[1])).abs() < 8.0);
}

#[test]
fn renders_grayscale_at_zero_saturation() {
    let generated = generate_system_theme_colors(&SystemThemeInput { saturation: Some(0.0), ..dracula() });
    let ColorValue::Text(error) = generated.colors.get(ThemeToken::Error) else { panic!("hex") };
    assert!(color_to_oklch(&parse_color(error).expect("hex")).c < 0.005);
}

#[test]
fn falls_back_to_palette_indices_and_faint_text_without_a_background() {
    let generated = generate_system_theme_colors(&SystemThemeInput {
        appearance_hint: Some(TerminalTheme::Light),
        ..SystemThemeInput::default()
    });
    assert_eq!(generated.appearance, Some(TerminalTheme::Light));
    assert_eq!(generated.colors.get(ThemeToken::Error), &ColorValue::Number(1.0));
    assert_eq!(generated.colors.get(ThemeToken::Text), &ColorValue::Text(String::new()));
    assert_eq!(generated.colors.get(ThemeToken::UserMessageBg), &ColorValue::Text(String::new()));
    assert!(generated.dim.contains(&ThemeColor::Muted));
    let gray = generate_system_theme_colors(&SystemThemeInput { saturation: Some(0.0), ..SystemThemeInput::default() });
    assert_eq!(gray.colors.get(ThemeToken::Error), &ColorValue::Text(String::new()));
}

#[test]
fn system_theme_is_listed_first_and_generated_from_the_terminal_colors() {
    let _state = global_state();
    assert_eq!(get_available_themes()[0], "system");
    assert_eq!(get_theme_export_colors(Some("system")), ThemeExportColors::default());
    let input = dracula();
    set_terminal_colors(TerminalColors {
        foreground: input.foreground,
        background: input.background,
        palette: input.palette,
    });
    let theme = get_theme_by_name("system").expect("system theme");
    assert_eq!(theme.appearance(), TerminalTheme::Dark);
    assert_eq!(theme.get_fg_ansi(ThemeColor::Text), "\x1b[39m");
    assert!(theme.get_fg_ansi(ThemeColor::Error).starts_with("\x1b[38;"));
}

#[test]
fn renders_faint_tokens_with_sgr_2_and_closes_it() {
    let _state = global_state();
    let theme = get_theme_by_name("system").expect("system theme");
    assert_eq!(theme.fg(ThemeColor::Muted, "x"), "\x1b[39m\x1b[2mx\x1b[22;39m");
    let style = ThemeStyle { fg: Some(StyleFg::Token(ThemeColor::Muted)), ..ThemeStyle::default() };
    assert_eq!(theme.style("x", &style), "\x1b[39m\x1b[2mx\x1b[22m\x1b[39m");
}
