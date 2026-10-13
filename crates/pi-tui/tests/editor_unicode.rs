//! Ports of the upstream editor tests: Unicode editing, word movement, and scroll indicators.
//! Cursor columns are byte offsets, so expected columns are written as prefix lengths.

mod editor_common;

use std::rc::Rc;

use editor_common::{cursor, default_theme, editor, editor_with, type_text};
use pi_tui::components::editor::{EditorOptions, EditorTheme};
use pi_tui::{strip_terminal_sequences, visible_width};

const LEFT: &str = "\x1b[D";
const CTRL_LEFT: &str = "\x1b[1;5D";
const CTRL_RIGHT: &str = "\x1b[1;5C";

#[test]
fn inserts_mixed_ascii_umlauts_and_emojis() {
    let editor = editor();
    type_text(&editor, "Hello äöü 😀");
    assert_eq!(editor.get_text(), "Hello äöü 😀");
}

#[test]
fn deletes_umlauts_with_backspace() {
    let editor = editor();
    type_text(&editor, "äöü");
    editor.handle_input("\x7f");
    assert_eq!(editor.get_text(), "äö");
}

#[test]
fn deletes_emojis_with_one_backspace() {
    let editor = editor();
    type_text(&editor, "😀👍");
    editor.handle_input("\x7f");
    assert_eq!(editor.get_text(), "😀");
}

#[test]
fn inserts_after_moving_over_umlauts() {
    let editor = editor();
    type_text(&editor, "äöü");
    editor.handle_input(LEFT);
    editor.handle_input(LEFT);
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "äxöü");
}

#[test]
fn moves_over_emojis_with_one_arrow_key() {
    let editor = editor();
    type_text(&editor, "😀👍🎉");
    editor.handle_input(LEFT);
    editor.handle_input(LEFT);
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "😀x👍🎉");
}

#[test]
fn preserves_umlauts_across_line_breaks() {
    let editor = editor();
    type_text(&editor, "äöü");
    editor.handle_input("\n");
    type_text(&editor, "ÄÖÜ");
    assert_eq!(editor.get_text(), "äöü\nÄÖÜ");
}

#[test]
fn replaces_the_document_with_set_text() {
    let editor = editor();
    editor.set_text("Hällö Wörld! 😀 äöüÄÖÜß");
    assert_eq!(editor.get_text(), "Hällö Wörld! 😀 äöüÄÖÜß");
}

#[test]
fn ctrl_a_moves_to_start() {
    let editor = editor();
    type_text(&editor, "ab");
    editor.handle_input("\x01");
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "xab");
}

#[test]
fn deletes_words_with_ctrl_w_and_alt_backspace() {
    let editor = editor();
    for (text, expected) in [
        ("foo bar baz", "foo bar "),
        ("foo bar   ", "foo "),
        ("foo bar...", "foo bar"),
        ("foo.bar", "foo."),
        ("foo:bar", "foo:"),
        ("line one\nline two", "line one\nline "),
        ("line one\n", "line one"),
    ] {
        editor.set_text(text);
        editor.handle_input("\x17");
        assert_eq!(editor.get_text(), expected, "{text:?}");
    }

    editor.set_text("foo 😀😀 bar");
    editor.handle_input("\x17");
    assert_eq!(editor.get_text(), "foo 😀😀 ");
    editor.handle_input("\x17");
    assert_eq!(editor.get_text(), "foo ");

    editor.set_text("foo bar");
    editor.handle_input("\x1b\x7f");
    assert_eq!(editor.get_text(), "foo ");
}

#[test]
fn navigates_words_with_ctrl_arrows() {
    let editor = editor();
    editor.set_text("foo bar... baz");
    for expected in [11, 7, 4] {
        editor.handle_input(CTRL_LEFT);
        assert_eq!(cursor(&editor), (0, expected));
    }
    for expected in [7, 10, 14] {
        editor.handle_input(CTRL_RIGHT);
        assert_eq!(cursor(&editor), (0, expected));
    }

    editor.set_text("   foo bar");
    editor.handle_input("\x01");
    editor.handle_input(CTRL_RIGHT);
    assert_eq!(cursor(&editor), (0, 6));

    editor.set_text("foo.bar baz");
    for expected in [8, 4, 3] {
        editor.handle_input(CTRL_LEFT);
        assert_eq!(cursor(&editor), (0, expected));
    }
    editor.handle_input("\x01");
    for expected in [3, 4, 7] {
        editor.handle_input(CTRL_RIGHT);
        assert_eq!(cursor(&editor), (0, expected));
    }
}

/// Upstream relies on ICU's dictionary, which groups 你好 and 世界 into words. UAX #29 has no
/// dictionary, so each ideograph is its own word; fullwidth punctuation still stops movement.
#[test]
fn stops_at_fullwidth_chinese_punctuation() {
    let editor = editor();
    let text = "你好，世界";
    editor.set_text(text);
    let at = |prefix: &str| (0, prefix.len());

    for expected in ["你好，世", "你好，", "你好", "你", ""] {
        editor.handle_input(CTRL_LEFT);
        assert_eq!(cursor(&editor), at(expected));
    }
    for expected in ["你", "你好", "你好，", "你好，世", text] {
        editor.handle_input(CTRL_RIGHT);
        assert_eq!(cursor(&editor), at(expected));
    }
}

#[test]
fn handles_mixed_cjk_and_ascii_word_movement() {
    let editor = editor();
    let text = "hello你好，world世界";
    editor.set_text(text);
    let at = |prefix: &str| (0, prefix.len());

    for expected in ["hello你好，world世", "hello你好，world", "hello你好，", "hello你好", "hello你", "hello", ""]
    {
        editor.handle_input(CTRL_LEFT);
        assert_eq!(cursor(&editor), at(expected), "left to {expected:?}");
    }
    for expected in ["hello", "hello你", "hello你好", "hello你好，", "hello你好，world", "hello你好，world世", text]
    {
        editor.handle_input(CTRL_RIGHT);
        assert_eq!(cursor(&editor), at(expected), "right to {expected:?}");
    }
}

fn twenty_lines() -> String {
    (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
}

#[test]
fn centers_scroll_indicators_on_wide_borders() {
    let width = 40;
    let editor = editor_with(width, default_theme(), EditorOptions::default());
    editor.set_text(&twenty_lines());
    editor.render(width);
    for _ in 0..10 {
        editor.handle_input("\x1b[A");
    }
    let lines = editor.render(width);
    let rule = "\u{2500}".repeat(15);
    assert_eq!(strip_terminal_sequences(&lines[0]), format!("{rule} \u{2191} 9 more {rule}"));
    assert_eq!(strip_terminal_sequences(lines.last().unwrap()), format!("{rule} \u{2193} 4 more {rule}"));
}

#[test]
fn keeps_truncated_scroll_indicators_in_width_and_colored() {
    let width = 10;
    let border_color: pi_tui::components::scroll_view::StyleFn =
        Rc::new(|text: &str| format!("\x1b[35m{text}\x1b[39m"));
    let theme = EditorTheme { border_color: border_color.clone(), ..default_theme() };
    let editor = editor_with(width, theme, EditorOptions::default());
    editor.set_text(&twenty_lines());
    editor.render(width);
    for _ in 0..10 {
        editor.handle_input("\x1b[A");
    }
    let lines = editor.render(width);
    let top = &lines[0];
    let bottom = lines.last().unwrap();
    assert!(strip_terminal_sequences(top).starts_with("\u{2500}\u{2500}\u{2500} \u{2191}"));
    assert!(strip_terminal_sequences(bottom).starts_with("\u{2500}\u{2500}\u{2500} \u{2193}"));
    assert_eq!(*top, border_color(&strip_terminal_sequences(top)));
    assert_eq!(*bottom, border_color(&strip_terminal_sequences(bottom)));
    for line in &lines {
        assert_eq!(visible_width(line), width, "line exceeds width: {line:?}");
    }
}

#[test]
fn custom_border_renderer_replaces_the_top_border() {
    let width = 20;
    let editor = editor_with(width, default_theme(), EditorOptions::default());
    editor.set_top_border_renderer(Some(Box::new(|context| {
        if context.hidden_line_count > 0 {
            return context.default_border();
        }
        format!("== status {}", "=".repeat(context.width - 10))
    })));
    let lines = editor.render(width);
    assert_eq!(lines[0], format!("== status {}", "=".repeat(10)));
    assert_eq!(strip_terminal_sequences(lines.last().unwrap()), "\u{2500}".repeat(width));
}
