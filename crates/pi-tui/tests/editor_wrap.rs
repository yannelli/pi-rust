//! Ports of the upstream editor tests: grapheme-aware wrapping and `wordWrapLine`.

mod editor_common;

use editor_common::{default_theme, editor, editor_with, type_text};
use pi_tui::components::editor::{EditorOptions, TextChunk, TextSegment, word_wrap_line, word_wrap_line_with_segments};
use pi_tui::{FAKE_CURSOR_START, render_fake_cursor, strip_terminal_sequences, visible_width};

fn content(lines: &[String]) -> &[String] {
    &lines[1..lines.len() - 1]
}

fn assert_content_width(lines: &[String], width: usize) {
    for (i, line) in content(lines).iter().enumerate() {
        assert_eq!(visible_width(line), width, "line {i}: {line:?}");
    }
}

fn texts(chunks: &[TextChunk]) -> Vec<&str> {
    chunks.iter().map(|c| c.text.as_str()).collect()
}

fn reconstruct(line: &str, chunks: &[TextChunk]) -> String {
    chunks.iter().map(|c| &line[c.start_index..c.end_index]).collect()
}

#[test]
fn wraps_text_with_wide_emojis() {
    let editor = editor();
    editor.set_text("Hello ✅ World");
    assert_content_width(&editor.render(20), 20);
}

#[test]
fn wraps_long_emoji_text() {
    let editor = editor();
    editor.set_text("✅✅✅✅✅✅");
    assert_content_width(&editor.render(10), 10);
}

#[test]
fn renders_isolated_thai_and_lao_am_without_drift() {
    for text in ["\u{0E33}abc", "\u{0EB3}abc"] {
        let editor = editor();
        editor.set_text(text);
        for line in editor.render(8) {
            assert_eq!(visible_width(&line), 8, "{text:?}: {line:?}");
        }
    }
}

#[test]
fn wraps_cjk_characters() {
    let editor = editor();
    let width = 11;
    editor.set_text("日本語テスト");
    let lines = editor.render(width);
    assert_content_width(&lines, width);
    let contents: Vec<String> =
        content(&lines).iter().map(|l| strip_terminal_sequences(l).trim().to_string()).collect();
    assert_eq!(contents, ["日本語テス", "ト"]);
}

#[test]
fn fits_mixed_ascii_and_wide_characters() {
    let editor = editor();
    editor.set_text("Test ✅ OK 日本");
    let lines = editor.render(16);
    assert_eq!(content(&lines).len(), 1);
    assert_eq!(visible_width(&content(&lines)[0]), 16);
}

#[test]
fn renders_cursor_on_wide_characters() {
    let editor = editor();
    editor.set_text("A✅B");
    let lines = editor.render(20);
    assert!(lines[1].contains(FAKE_CURSOR_START));
    assert_eq!(visible_width(&lines[1]), 20);
}

#[test]
fn does_not_exceed_width_with_emoji_at_wrap_boundary() {
    let editor = editor();
    editor.set_text("0123456789✅");
    for line in content(&editor.render(11)) {
        assert!(visible_width(line) <= 11, "{line:?}");
    }
}

#[test]
fn shows_cursor_at_end_before_wrap_then_wraps() {
    let width = 10;
    for padding_x in [0, 1] {
        let options = EditorOptions { padding_x, ..EditorOptions::default() };
        let editor = editor_with(width + padding_x, default_theme(), options);
        type_text(&editor, "aaaaaaaaa");
        let lines = editor.render(width + padding_x);
        assert_eq!(content(&lines).len(), 1);
        assert!(content(&lines)[0].ends_with(&render_fake_cursor(" ")), "{:?}", content(&lines)[0]);
        editor.handle_input("a");
        assert_eq!(content(&editor.render(width + padding_x)).len(), 2);
    }
}

#[test]
fn wraps_at_word_boundaries() {
    let editor = editor();
    editor.set_text("Hello world this is a test of word wrapping functionality");
    let lines = editor.render(40);
    for line in content(&lines) {
        let line = strip_terminal_sequences(line);
        let last = line.trim_end().chars().last();
        assert!(last.is_none_or(|c| c.is_alphanumeric() || ".,!?;:_".contains(c)), "{line:?}");
    }
}

#[test]
fn does_not_start_wrapped_lines_with_whitespace() {
    let editor = editor();
    editor.set_text("Word1 Word2 Word3 Word4 Word5 Word6");
    for line in content(&editor.render(20)) {
        let line = strip_terminal_sequences(line);
        if !line.trim_start().is_empty() {
            assert!(!line.starts_with(' '), "{line:?}");
        }
    }
}

#[test]
fn breaks_long_words_by_character() {
    let editor = editor();
    editor.set_text("Check https://example.com/very/long/path/that/exceeds/width here");
    assert_content_width(&editor.render(30), 30);
}

#[test]
fn keeps_multiple_spaces() {
    let editor = editor();
    editor.set_text("Word1   Word2    Word3");
    let lines = editor.render(50);
    assert!(strip_terminal_sequences(&lines[1]).contains("Word1   Word2"));
}

#[test]
fn renders_empty_text_as_three_lines() {
    let editor = editor();
    editor.set_text("");
    assert_eq!(editor.render(40).len(), 3);
}

#[test]
fn fits_a_word_of_exact_width() {
    let editor = editor();
    editor.set_text("1234567890");
    let lines = editor.render(11);
    assert_eq!(lines.len(), 3);
    assert!(strip_terminal_sequences(&lines[1]).contains("1234567890"));
}

#[test]
fn word_wrap_line_cases() {
    let cases: &[(&str, usize, &[&str])] = &[
        ("hello world test", 11, &["hello ", "world test"]),
        ("hello world test", 12, &["hello world ", "test"]),
        ("aaaaaaaaaaaa aaaa", 12, &["aaaaaaaaaaaa", " aaaa"]),
        ("      aaaaaaaaaaaa", 12, &["      ", "aaaaaaaaaaaa"]),
        ("Lorem ipsum dolor sit amet,    consectetur", 30, &["Lorem ipsum dolor sit ", "amet,    consectetur"]),
        (
            "Lorem ipsum dolor sit amet,              consectetur",
            30,
            &["Lorem ipsum dolor sit ", "amet,              consectetur"],
        ),
        (
            "Lorem ipsum dolor sit amet,               consectetur",
            30,
            &["Lorem ipsum dolor sit ", "amet,               ", "consectetur"],
        ),
        (
            "Lorem ipsum dolor sit amet,                         consectetur",
            30,
            &["Lorem ipsum dolor sit ", "amet,                         ", "consectetur"],
        ),
        (
            "Lorem ipsum dolor sit amet,                          consectetur",
            30,
            &["Lorem ipsum dolor sit ", "amet,                         ", " consectetur"],
        ),
        (
            "Lorem ipsum dolor sit amet,                                     consectetur",
            30,
            &["Lorem ipsum dolor sit ", "amet,                         ", "            consectetur"],
        ),
    ];
    for (line, width, expected) in cases {
        assert_eq!(texts(&word_wrap_line(line, *width)), *expected, "{line:?} at {width}");
    }
}

#[test]
fn force_breaks_when_wide_char_still_overflows() {
    let line = format!(" {}你", "a".repeat(186));
    let chunks = word_wrap_line(&line, 187);
    for chunk in &chunks {
        assert!(visible_width(&chunk.text) <= 187);
    }
    assert_eq!(reconstruct(&line, &chunks), line);
}

const MARKER: &str = "[paste #1 +20 lines]";

fn segments_for<'a>(parts: &[&'a str]) -> Vec<TextSegment<'a>> {
    let mut index = 0;
    parts
        .iter()
        .map(|part| {
            let segment = TextSegment { segment: part, index };
            index += part.len();
            segment
        })
        .collect()
}

fn wrap_parts(parts: &[&str], width: usize) -> (String, Vec<TextChunk>) {
    let line = parts.concat();
    let segments = segments_for(parts);
    let chunks = word_wrap_line_with_segments(&line, width, Some(&segments));
    for chunk in &chunks {
        assert!(visible_width(&chunk.text) <= width, "{:?}", chunk.text);
    }
    assert_eq!(reconstruct(&line, &chunks), line);
    (line, chunks)
}

#[test]
fn splits_oversized_atomic_segments() {
    wrap_parts(&["A", MARKER, "B"], 10);

    let (_, chunks) = wrap_parts(&[MARKER, "B"], 10);
    assert!(chunks.last().unwrap().text.contains('B'));

    let (_, chunks) = wrap_parts(&["A", MARKER], 10);
    assert_eq!(chunks[0].text, "A");

    wrap_parts(&[MARKER, "[paste #2 +30 lines]"], 10);
}

#[test]
fn wraps_normally_after_an_oversized_atomic_segment() {
    let mut parts = vec![MARKER, " "];
    parts.extend(["h", "e", "l", "l", "o", " ", "w", "o", "r", "l", "d"]);
    let (_, chunks) = wrap_parts(&parts, 10);
    assert_eq!(chunks.last().unwrap().text, "world");
}

#[test]
fn single_wide_grapheme_wider_than_the_line_does_not_recurse() {
    let chunks = word_wrap_line("你好", 1);
    assert_eq!(texts(&chunks), ["你", "好"]);
}
