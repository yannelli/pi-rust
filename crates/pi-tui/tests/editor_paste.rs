//! Ports of the upstream editor paste marker tests.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;

use editor_common::{TestEditor, UNDO, cursor, editor, editor_cols, type_text};
use pi_tui::visible_width;
use regex::Regex;

const RIGHT: &str = "\x1b[C";
const LEFT: &str = "\x1b[D";
const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const CTRL_A: &str = "\x01";

fn paste(editor: &TestEditor, content: &str) {
    editor.handle_input(&format!("\x1b[200~{content}\x1b[201~"));
}

/// Upstream `pasteWithMarker`: a 20-line paste.
fn paste_with_marker(editor: &TestEditor) {
    paste(editor, "line\n".repeat(20).trim_end());
}

/// Upstream `bigPaste`: 12 tagged lines.
fn big_paste(tag: &str) -> String {
    (0..12).map(|i| format!("{tag}{i}")).collect::<Vec<_>>().join("\n")
}

fn markers(text: &str) -> Vec<String> {
    let regex = Regex::new(r"\[paste #\d+ (\+\d+ lines|\d+ chars)\]").unwrap();
    regex.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

fn submitted(editor: &TestEditor) -> Rc<RefCell<String>> {
    let submitted = Rc::new(RefCell::new(String::new()));
    let sink = submitted.clone();
    editor.on_submit(move |text| *sink.borrow_mut() = text.to_string());
    submitted
}

/// "A", a marker, then "B"; returns the marker.
fn a_marker_b(editor: &TestEditor) -> String {
    editor.handle_input("A");
    paste_with_marker(editor);
    editor.handle_input("B");
    markers(&editor.get_text()).remove(0)
}

#[test]
fn creates_a_marker_for_large_pastes() {
    let editor = editor();
    paste_with_marker(&editor);
    assert_eq!(markers(&editor.get_text()).len(), 1);
}

#[test]
fn right_arrow_skips_the_marker() {
    let editor = editor();
    let marker = a_marker_b(&editor);
    editor.handle_input(CTRL_A);
    for expected in [1, 1 + marker.len(), 2 + marker.len()] {
        editor.handle_input(RIGHT);
        assert_eq!(cursor(&editor), (0, expected));
    }
}

#[test]
fn left_arrow_skips_the_marker() {
    let editor = editor();
    let marker = a_marker_b(&editor);
    for expected in [1 + marker.len(), 1, 0] {
        editor.handle_input(LEFT);
        assert_eq!(cursor(&editor), (0, expected));
    }
}

#[test]
fn backspace_deletes_the_whole_marker() {
    let editor = editor();
    let marker = a_marker_b(&editor);
    editor.handle_input(CTRL_A);
    editor.handle_input(RIGHT);
    editor.handle_input(RIGHT);
    assert_eq!(cursor(&editor), (0, 1 + marker.len()));
    editor.handle_input("\x7f");
    assert_eq!(editor.get_text(), "AB");
    assert_eq!(cursor(&editor), (0, 1));
}

#[test]
fn forward_delete_deletes_the_whole_marker() {
    let editor = editor();
    a_marker_b(&editor);
    editor.handle_input(CTRL_A);
    editor.handle_input(RIGHT);
    editor.handle_input("\x1b[3~");
    assert_eq!(editor.get_text(), "AB");
    assert_eq!(cursor(&editor), (0, 1));
}

#[test]
fn word_movement_skips_the_marker() {
    let editor = editor();
    type_text(&editor, "X ");
    paste_with_marker(&editor);
    type_text(&editor, " Y");
    let marker = markers(&editor.get_text()).remove(0);
    editor.handle_input(CTRL_A);
    editor.handle_input("\x1b[1;5C");
    assert_eq!(cursor(&editor), (0, 1));
    editor.handle_input("\x1b[1;5C");
    assert_eq!(cursor(&editor), (0, 2 + marker.len()));
}

#[test]
fn undo_restores_a_deleted_marker() {
    let editor = editor();
    a_marker_b(&editor);
    let before = editor.get_text();
    editor.handle_input(CTRL_A);
    editor.handle_input(RIGHT);
    editor.handle_input(RIGHT);
    editor.handle_input("\x7f");
    assert_eq!(editor.get_text(), "AB");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), before);
}

#[test]
fn undo_restores_the_paste_registry() {
    let editor = editor();
    let submitted = submitted(&editor);
    let content = big_paste("alpha");
    paste(&editor, &content);
    editor.handle_input("\x7f");
    editor.handle_input(UNDO);
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), content);
}

#[test]
fn undo_restores_both_registry_entries() {
    let editor = editor();
    let submitted = submitted(&editor);
    let (a, b) = (big_paste("alpha"), big_paste("beta"));
    paste(&editor, &a);
    paste(&editor, &b);
    editor.handle_input(CTRL_A);
    editor.handle_input(RIGHT);
    editor.handle_input("\x7f");
    editor.handle_input(UNDO);
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), format!("{a}{b}"));
}

#[test]
fn renumbers_the_registry_in_id_order() {
    let editor = editor();
    let submitted = submitted(&editor);
    let (a, b, c) = (big_paste("alpha"), big_paste("beta"), big_paste("gamma"));
    paste(&editor, &a);
    editor.handle_input(CTRL_A);
    paste(&editor, &b);
    editor.handle_input(CTRL_A);
    paste(&editor, &c);
    editor.handle_input("\x05");
    editor.handle_input("\x7f");
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), format!("{c}{b}"));
}

#[test]
fn renumbering_keeps_the_cursor_after_shorter_markers() {
    let editor = editor();
    let submitted = submitted(&editor);
    for i in 1..=10 {
        paste(&editor, &big_paste(&format!("p{i}-")));
        editor.handle_input(CTRL_A);
    }
    // Markers #10 down to #1; delete #1 at the end of the line.
    editor.handle_input("\x05");
    editor.handle_input("\x7f");
    assert!(editor.get_text().starts_with("[paste #9 +12 lines]"));
    assert_eq!(cursor(&editor), (0, editor.get_text().len()));
    editor.handle_input("Z");
    editor.handle_input("\r");
    assert!(submitted.borrow().ends_with("p2-11Z"));
}

#[test]
fn undo_after_set_text_restores_markers() {
    let editor = editor();
    let submitted = submitted(&editor);
    let content = big_paste("alpha");
    paste(&editor, &content);
    editor.set_text("replacement");
    editor.handle_input(UNDO);
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), content);
}

#[test]
fn handles_several_markers_on_one_line() {
    let editor = editor();
    paste_with_marker(&editor);
    editor.handle_input(" ");
    paste_with_marker(&editor);
    let found = markers(&editor.get_text());
    assert_eq!(found.len(), 2);
    editor.handle_input(CTRL_A);
    for expected in [found[0].len(), found[0].len() + 1, found[0].len() + 1 + found[1].len()] {
        editor.handle_input(RIGHT);
        assert_eq!(cursor(&editor), (0, expected));
    }
}

#[test]
fn typed_marker_text_is_not_atomic() {
    let editor = editor();
    let fake = "[paste #99 +5 lines]";
    type_text(&editor, fake);
    assert_eq!(editor.get_text(), fake);
    editor.handle_input(CTRL_A);
    editor.handle_input(RIGHT);
    assert_eq!(cursor(&editor), (0, 1));
}

fn assert_within(lines: &[String], width: usize) {
    for line in lines {
        assert!(visible_width(line) <= width, "line exceeds {width}: {line:?}");
    }
}

#[test]
fn markers_wider_than_the_terminal_do_not_overflow() {
    let editor = editor();
    paste(&editor, "line\n".repeat(47).trim_end());
    let marker = markers(&editor.get_text()).remove(0);
    assert!(visible_width(&marker) > 8);
    assert_within(&editor.render(8), 8);
}

#[test]
fn text_and_marker_wider_than_the_terminal_do_not_overflow() {
    let editor = editor();
    type_text(&editor, &"b".repeat(35));
    paste(&editor, "line\n".repeat(27).trim_end());
    type_text(&editor, "bbbb");
    for _ in 0..5 {
        editor.handle_input(LEFT);
    }
    assert_within(&editor.render(54), 54);
}

#[test]
fn wrapping_rechecks_overflow_after_backtracking() {
    let editor = editor();
    editor.handle_input(" ");
    type_text(&editor, &"b".repeat(35));
    paste(&editor, "line\n".repeat(27).trim_end());
    type_text(&editor, "bbbb");
    assert_within(&editor.render(54), 54);
}

const LITERAL: &str =
    "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nline 9\nline 10\ntokens $1 $2 $& $$ $` $' end";

#[test]
fn expands_pasted_content_literally() {
    let editor = editor();
    paste(&editor, LITERAL);
    assert_eq!(markers(&editor.get_text()).len(), 1);
    assert_eq!(editor.get_expanded_text(), LITERAL);
}

#[test]
fn submits_pasted_content_literally() {
    let editor = editor();
    let submitted = submitted(&editor);
    paste(&editor, LITERAL);
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), LITERAL);
}

#[test]
fn snaps_to_the_marker_start_moving_down() {
    let editor = editor();
    editor.set_text("12345678901234567890\n\nhello ");
    paste(&editor, &"x".repeat(2000));
    editor.render(80);
    assert_eq!(markers(&editor.get_text()).len(), 1);
    editor.handle_input(UP);
    editor.handle_input(UP);
    editor.handle_input(CTRL_A);
    for _ in 0..10 {
        editor.handle_input(RIGHT);
    }
    assert_eq!(cursor(&editor), (0, 10));
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor), (1, 0));
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor), (2, 6));
}

#[test]
fn keeps_the_sticky_column_through_a_marker_line() {
    let editor = editor_cols(30);
    type_text(&editor, "1234567890123456\n\n");
    paste(&editor, &"x".repeat(2000));
    type_text(&editor, "\n\nabcdefghijklmnop");
    editor.render(30);
    for _ in 0..4 {
        editor.handle_input(UP);
    }
    editor.handle_input(CTRL_A);
    for _ in 0..10 {
        editor.handle_input(RIGHT);
    }
    assert_eq!(cursor(&editor), (0, 10));
    for expected in [(1, 0), (2, 0), (3, 0), (4, 10)] {
        editor.handle_input(DOWN);
        assert_eq!(cursor(&editor), expected);
    }
}

/// Line 0: `abcdefgh` + a 21-column marker + `ijklmnopqr`; line 1: 18 digits; width 20.
fn wide_marker_editor() -> TestEditor {
    let editor = editor_cols(20);
    type_text(&editor, "abcdefgh");
    paste(&editor, "line\n".repeat(100).trim_end());
    type_text(&editor, "ijklmnopqr\n123456789012345678");
    editor.render(20);
    editor
}

#[test]
fn does_not_get_stuck_moving_down_from_a_wide_marker() {
    let editor = wide_marker_editor();
    let marker = markers(&editor.get_text()).remove(0);
    assert!(marker.len() > 20);
    let (start, end) = (8, 8 + marker.len());
    editor.handle_input(UP);
    editor.handle_input(CTRL_A);
    for _ in 0..6 {
        editor.handle_input(RIGHT);
    }
    assert_eq!(cursor(&editor), (0, 6));
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor), (0, start));
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor), (0, end));
    editor.handle_input(UP);
    assert_eq!(cursor(&editor), (0, start));
    editor.handle_input(UP);
    assert_eq!(cursor(&editor), (0, 6));
}

#[test]
fn skips_marker_continuation_lines() {
    let editor = wide_marker_editor();
    editor.handle_input(UP);
    editor.handle_input(CTRL_A);
    for _ in 0..3 {
        editor.handle_input(RIGHT);
    }
    assert_eq!(cursor(&editor), (0, 3));
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor).1, 8);
    editor.handle_input(DOWN);
    assert_eq!(cursor(&editor), (1, 3));
    editor.handle_input(UP);
    assert_eq!(cursor(&editor).1, 8);
    editor.handle_input(UP);
    assert_eq!(cursor(&editor), (0, 3));
}

#[test]
fn pasted_path_after_a_word_gets_a_space() {
    let editor = editor();
    type_text(&editor, "see");
    paste(&editor, "/tmp/file.txt");
    assert_eq!(editor.get_text(), "see /tmp/file.txt");
}
