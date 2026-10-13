//! Unit tests for key mermaid cases; the full upstream corpus lives in
//! `tests/mermaid_render.rs`.

use super::labels::{decode_html_entities, fit_label, wrap_label};
use super::width::string_width;
use super::*;

fn plain(src: &str) -> Vec<String> {
    render(src).expect("diagram renders").plain
}

#[test]
fn renders_lr_flowchart() {
    let art = render("flowchart LR\n  A[Start] --> B[Done]").expect("renders");
    assert_eq!(art.plain, ["┌───────┐    ┌──────┐", "│ Start ├───▶│ Done │", "└───────┘    └──────┘"]);
    assert_eq!(art.width, 21);
    assert!(art.warnings.is_empty());
    let middle: Vec<(&str, &str)> = art.styled[1].iter().map(|s| (s.text.as_str(), s.cls.as_str())).collect();
    assert_eq!(
        middle,
        [
            ("│", "border"),
            (" ", "none"),
            ("Start", "text"),
            (" ", "none"),
            ("├", "border"),
            ("───▶", "edge"),
            ("│", "border"),
            (" ", "none"),
            ("Done", "text"),
            (" ", "none"),
            ("│", "border"),
        ]
    );
}

#[test]
fn styled_rows_join_to_plain_rows() {
    let art = render("graph TD\n  A -->|yes| B\n  A -.-> C\n  B ==> C").expect("renders");
    for (row, spans) in art.plain.iter().zip(&art.styled) {
        let joined: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined.trim_end_matches(' '), row);
    }
}

#[test]
fn strict_grammar_retries_without_final_line() {
    let art = render("stateDiagram-v2\n A --> B\n some garbage line").expect("salvaged");
    assert_eq!(art.plain, [" ╭───╮", " │ A │", " ╰─┬─╯", "   │", "   ▼", " ╭───╮", " │ B │", " ╰───╯"]);
    assert_eq!(art.warnings, ["dropped, unreadable final line: \"some garbage line\""]);
    assert_eq!(render("stateDiagram-v2\n bad line here\n A --> B"), None);
}

#[test]
fn flowchart_keeps_parsed_prefix_and_warns() {
    let art = render("graph TD\n A[Start --> B").expect("renders");
    assert_eq!(art.plain, [" ┌─────────────┐", " │ Start --> B │", " └─────────────┘"]);
    assert_eq!(art.warnings, ["node \"A\": label is missing its closing `]`"]);
}

#[test]
fn sequence_message_with_label() {
    assert_eq!(
        plain("sequenceDiagram\n A->>B: hi"),
        [
            "┌───┐  ┌───┐",
            "│ A │  │ B │",
            "└─┬─┘  └─┬─┘",
            "  │      │",
            "  │  hi  │",
            "  ├─────▶│",
            "  │      │",
            "┌─┴─┐  ┌─┴─┐",
            "│ A │  │ B │",
            "└───┘  └───┘",
        ]
    );
}

#[test]
fn unsupported_and_blank_sources_render_nothing() {
    assert_eq!(render(""), None);
    assert_eq!(render("  \n\t "), None);
    assert_eq!(render("pie title Pets\n  \"Dogs\" : 386"), None);
    assert_eq!(render("graph TD"), None);
}

#[test]
fn diagram_kind_reads_the_header() {
    assert_eq!(diagram_kind("graph LR\nA-->B"), Some(DiagramKind::Flowchart));
    assert_eq!(diagram_kind("FLOWCHART TD"), Some(DiagramKind::Flowchart));
    assert_eq!(diagram_kind("stateDiagram-v2"), Some(DiagramKind::State));
    assert_eq!(diagram_kind("classDiagram"), Some(DiagramKind::Class));
    assert_eq!(diagram_kind("erDiagram"), Some(DiagramKind::Er));
    assert_eq!(diagram_kind("%% note\nsequenceDiagram"), Some(DiagramKind::Sequence));
    assert_eq!(diagram_kind("pie"), None);
    assert_eq!(diagram_kind(""), None);
    assert_eq!(DiagramKind::Er.as_str(), "er");
}

#[test]
fn source_box_wraps_and_titles() {
    let art = source_box("pie\n  \"a\" : 1", Some(20));
    assert_eq!(art.plain, ["╭ mermaid: pie ──╮", "│ pie            │", "│   \"a\" : 1      │", "╰────────────────╯"]);
    assert_eq!(art.width, 18);
    assert_eq!(art.styled[0][1], Span { text: " mermaid: pie ".into(), cls: Cls::Title });
    // The body wraps at the 8-column floor; the title makes the box wider.
    assert_eq!(
        source_box("graph LR\n  A --> B --> C --> D", Some(0)).plain,
        [
            "╭ mermaid: graph ──╮",
            "│ graph LR         │",
            "│   A -->          │",
            "│ B --> C          │",
            "│ --> D            │",
            "╰──────────────────╯",
        ]
    );
}

#[test]
fn ansi_uses_theme_per_class() {
    let art = render("graph LR\n A --> B").expect("renders");
    assert_eq!(
        to_ansi(&art, &DEFAULT_THEME),
        [
            "\x1b[2m┌───┐\x1b[0m    \x1b[2m┌───┐\x1b[0m",
            "\x1b[2m│\x1b[0m A \x1b[2m├\x1b[0m\x1b[36m───▶\x1b[0m\x1b[2m│\x1b[0m B \x1b[2m│\x1b[0m",
            "\x1b[2m└───┘\x1b[0m    \x1b[2m└───┘\x1b[0m",
        ]
    );
    let bare = AnsiTheme { border: None, text: None, edge: None, edge_label: None, title: None, none: None };
    assert_eq!(to_ansi(&art, &bare), art.plain);
    assert_eq!(AnsiTheme::default(), DEFAULT_THEME);
}

#[test]
fn class_names_match_upstream() {
    let names: Vec<&str> =
        [Cls::Border, Cls::Text, Cls::Edge, Cls::EdgeLabel, Cls::Title, Cls::None].iter().map(|c| c.as_str()).collect();
    assert_eq!(names, ["border", "text", "edge", "edgeLabel", "title", "none"]);
}

#[test]
fn entities_decode_in_one_pass() {
    assert_eq!(decode_html_entities("&amp;lt; &#x41;&#66; &#0; & x"), "&lt; AB &#0; & x");
    assert_eq!(decode_html_entities("&verylongname; &gt"), "&verylongname; &gt");
}

#[test]
fn labels_wrap_and_truncate_by_display_width() {
    assert_eq!(wrap_label("alpha beta gamma", 10, 4), ["alpha beta", "gamma"]);
    assert_eq!(wrap_label("some_long_identifier_name", 10, 4), ["some_long_", "identifier", "_name"]);
    assert_eq!(wrap_label("a b c d e", 1, 2), ["a", "b\u{2026}"]);
    assert_eq!(fit_label("中文标签", 5), "中文\u{2026}");
    assert_eq!(string_width("👨‍👩‍👧 é"), 4);
}
