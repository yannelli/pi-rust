//! Highlighting tests: upstream `syntax-highlight.test.ts` cases and one snapshot per eager language.
//! Snapshots show each run as `«themeToken:text»`. Set `PI_UPDATE_SNAPSHOTS=1` to rewrite them.

use std::path::PathBuf;

use super::*;
use crate::theme::{CLI_HIGHLIGHT_CLASSES, HighlightStyle, ThemeColor, highlight_code, init_theme, theme};

/// The 21 languages upstream registers at startup, with their sample file extension.
const EAGER_LANGUAGES: &[(&str, &str)] = &[
    ("python", "py"),
    ("java", "java"),
    ("go", "go"),
    ("javascript", "js"),
    ("json", "json"),
    ("cpp", "cpp"),
    ("typescript", "ts"),
    ("php", "php"),
    ("ruby", "rb"),
    ("c", "c"),
    ("csharp", "cs"),
    ("nix", "nix"),
    ("bash", "sh"),
    ("rust", "rs"),
    ("scala", "scala"),
    ("kotlin", "kt"),
    ("swift", "swift"),
    ("dart", "dart"),
    ("groovy", "groovy"),
    ("perl", "pl"),
    ("lua", "lua"),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/highlight")
}

/// A highlight theme that names the theme token each class maps to.
fn token_theme() -> HighlightTheme {
    let mut theme = HighlightTheme::new();
    for &(class, style) in CLI_HIGHLIGHT_CLASSES {
        let label = match style {
            HighlightStyle::Fg(color) => color.name(),
            HighlightStyle::Bold => "bold",
            HighlightStyle::Italic => "italic",
            HighlightStyle::Underline => "underline",
        };
        theme.insert(class, Arc::new(move |text: &str| format!("«{label}:{text}»")));
    }
    theme
}

#[test]
fn eager_languages_and_aliases_are_supported() {
    for (language, extension) in EAGER_LANGUAGES {
        assert!(supports_language(language), "{language}");
        assert!(supports_language(&language.to_uppercase()), "{language}");
        if *extension != "pl" {
            assert!(supports_language(extension), "{extension}");
        }
    }
    for alias in ["ts", "tsx", "golang", "c++", "c#", "sh", "zsh", "rs", "kt", "yml", "diff", "html", "text"] {
        assert!(supports_language(alias), "{alias}");
    }
    assert!(!supports_language("mermaid"));
    assert!(!supports_language("not-a-language"));
}

#[test]
fn renders_classes_through_the_theme_and_inherits_parent_formatting() {
    let theme = HighlightTheme::new().with("keyword", |text| format!("[keyword:{text}]"));
    let tokens = [
        HighlightToken { text: "const".into(), class: Some("keyword") },
        HighlightToken { text: " value".into(), class: None },
        HighlightToken { text: "x".into(), class: Some("keyword.other") },
        HighlightToken { text: "y".into(), class: Some("keyword-like") },
    ];
    assert_eq!(render_tokens(&tokens, &theme), "[keyword:const] value[keyword:x][keyword:y]");
}

#[test]
fn highlights_code_through_the_grammars() {
    let theme = HighlightTheme::new()
        .with("keyword", |text| format!("[keyword:{text}]"))
        .with("number", |text| format!("[number:{text}]"));
    let options = HighlightOptions { language: Some("typescript"), ignore_illegals: true, theme: Some(&theme) };
    let rendered = highlight("const value = 1", &options).expect("highlight");
    assert!(rendered.contains("[keyword:const]"), "{rendered}");
    assert!(rendered.contains("[number:1]"), "{rendered}");
    let unknown = HighlightOptions { language: Some("nope"), ..options };
    assert!(highlight("x", &unknown).is_err());
}

#[test]
fn colors_each_line_of_python_docstrings_independently() {
    let _state = crate::theme::tests::global_state();
    let code = "\"\"\"\nline one\n\nline two\n\"\"\"\nafter";
    let ansi = theme().get_fg_ansi(ThemeColor::SyntaxString);
    let expected = vec![
        format!("{ansi}\"\"\"\x1b[39m"),
        format!("{ansi}line one\x1b[39m"),
        String::new(),
        format!("{ansi}line two\x1b[39m"),
        format!("{ansi}\"\"\"\x1b[39m"),
        "after".to_string(),
    ];
    assert_eq!(highlight_code(code, Some("python")), expected);
    let markdown = crate::theme::get_markdown_theme();
    assert_eq!((markdown.highlight_code.expect("highlighter"))(code, Some("python")), expected);
}

#[test]
fn colors_diff_additions_and_deletions() {
    let _state = crate::theme::tests::global_state();
    let lines = highlight_code("-old\n+new\n", Some("diff"));
    assert_eq!(lines[0], format!("{}-old\x1b[39m", theme().get_fg_ansi(ThemeColor::ToolDiffRemoved)));
    assert_eq!(lines[1], format!("{}+new\x1b[39m", theme().get_fg_ansi(ThemeColor::ToolDiffAdded)));
}

#[test]
fn maps_regexes_decorators_and_tags_like_upstream() {
    let _state = crate::theme::tests::global_state();
    let fg = |color| theme().get_fg_ansi(color);
    let regex = &highlight_code("const re = /foo+/gi;", Some("javascript"))[0];
    assert!(regex.contains(&format!("{}/foo+/gi\x1b[39m", fg(ThemeColor::SyntaxString))), "{regex:?}");
    assert_eq!(highlight_code("@decorator", Some("python"))[0], format!("{}@decorator\x1b[39m", fg(ThemeColor::Muted)));
    let html = &highlight_code("<div></div>", Some("html"))[0];
    assert!(html.contains(&format!("{}div\x1b[39m", fg(ThemeColor::SyntaxKeyword))), "{html:?}");
}

#[test]
fn unsupported_languages_use_the_code_block_color() {
    let _state = crate::theme::tests::global_state();
    init_theme(Some("dark"), false);
    let lines = highlight_code("A --> B\nC", Some("mermaid"));
    let ansi = theme().get_fg_ansi(ThemeColor::MdCodeBlock);
    assert_eq!(lines, vec![format!("{ansi}A --> B\x1b[39m"), format!("{ansi}C\x1b[39m")]);
    assert_eq!(highlight_code("x", None), vec![format!("{ansi}x\x1b[39m")]);
}

#[test]
fn snapshot_of_each_eager_language() {
    let update = std::env::var("PI_UPDATE_SNAPSHOTS").is_ok_and(|value| value == "1");
    let theme = token_theme();
    let mut failures = Vec::new();
    for (language, extension) in EAGER_LANGUAGES {
        let source = std::fs::read_to_string(fixtures().join(format!("{language}.{extension}"))).expect("sample");
        let options = HighlightOptions { language: Some(language), ignore_illegals: true, theme: Some(&theme) };
        let rendered = highlight(&source, &options).expect("highlight");
        let snapshot = fixtures().join(format!("{language}.snap"));
        if update {
            std::fs::write(&snapshot, &rendered).expect("write snapshot");
            continue;
        }
        let expected = std::fs::read_to_string(&snapshot).unwrap_or_default();
        if rendered != expected {
            failures.push(format!("{language}:\n{rendered}"));
        }
    }
    assert!(failures.is_empty(), "snapshots differ (PI_UPDATE_SNAPSHOTS=1 rewrites them):\n{}", failures.join("\n"));
}
