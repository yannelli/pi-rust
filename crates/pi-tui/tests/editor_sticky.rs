//! Ports of the upstream editor sticky column tests.

mod editor_common;

use editor_common::{TestEditor, UNDO, cursor, editor, editor_cols};

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const RIGHT: &str = "\x1b[C";
const LEFT: &str = "\x1b[D";
const CTRL_A: &str = "\x01";
const CTRL_E: &str = "\x05";

fn press(editor: &TestEditor, key: &str, times: usize) {
    for _ in 0..times {
        editor.handle_input(key);
    }
}

/// Sends `key` and checks the cursor.
fn step(editor: &TestEditor, key: &str, expected: (usize, usize)) {
    editor.handle_input(key);
    assert_eq!(cursor(editor), expected, "after {key:?}");
}

fn line_start_then_right(editor: &TestEditor, cols: usize) {
    editor.handle_input(CTRL_A);
    press(editor, RIGHT, cols);
}

/// Upstream `positionCursor`: top of the text, down `line` lines, right `col` columns.
fn position_cursor(editor: &TestEditor, line: usize, col: usize) {
    press(editor, UP, 20);
    press(editor, DOWN, line);
    line_start_then_right(editor, col);
}

#[test]
fn keeps_the_column_moving_up_through_a_shorter_line() {
    let editor = editor();
    editor.set_text("2222222222x222\n\n1111111111_111111111111");
    assert_eq!(cursor(&editor), (2, 23));
    line_start_then_right(&editor, 10);
    assert_eq!(cursor(&editor), (2, 10));
    step(&editor, UP, (1, 0));
    step(&editor, UP, (0, 10));
}

#[test]
fn keeps_the_column_moving_down_through_a_shorter_line() {
    let editor = editor();
    editor.set_text("1111111111_111\n\n2222222222x222222222222");
    press(&editor, UP, 2);
    line_start_then_right(&editor, 10);
    assert_eq!(cursor(&editor), (0, 10));
    step(&editor, DOWN, (1, 0));
    step(&editor, DOWN, (2, 10));
}

#[test]
fn left_arrow_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    line_start_then_right(&editor, 5);
    assert_eq!(cursor(&editor), (2, 5));
    editor.handle_input(UP);
    step(&editor, UP, (0, 5));
    step(&editor, LEFT, (0, 4));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 4));
}

#[test]
fn right_arrow_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    press(&editor, UP, 2);
    line_start_then_right(&editor, 5);
    assert_eq!(cursor(&editor), (0, 5));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 5));
    step(&editor, RIGHT, (2, 6));
    editor.handle_input(UP);
    step(&editor, UP, (0, 6));
}

#[test]
fn typing_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    line_start_then_right(&editor, 8);
    editor.handle_input(UP);
    step(&editor, UP, (0, 8));
    step(&editor, "X", (0, 9));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 9));
}

#[test]
fn backspace_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    line_start_then_right(&editor, 8);
    editor.handle_input(UP);
    step(&editor, UP, (0, 8));
    step(&editor, "\x7f", (0, 7));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 7));
}

#[test]
fn ctrl_a_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    line_start_then_right(&editor, 8);
    editor.handle_input(UP);
    step(&editor, CTRL_A, (1, 0));
    step(&editor, UP, (0, 0));
}

#[test]
fn ctrl_e_resets_the_column() {
    let editor = editor();
    editor.set_text("12345\n\n1234567890");
    line_start_then_right(&editor, 3);
    editor.handle_input(UP);
    step(&editor, UP, (0, 3));
    step(&editor, CTRL_E, (0, 5));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 5));
}

#[test]
fn word_movement_resets_the_column() {
    let editor = editor();
    editor.set_text("hello world\n\nhello world");
    assert_eq!(cursor(&editor), (2, 11));
    editor.handle_input(UP);
    step(&editor, UP, (0, 11));
    step(&editor, "\x1b[1;5D", (0, 6));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 6));

    let editor = editor_common::editor();
    editor.set_text("hello world\n\nhello world");
    press(&editor, UP, 2);
    step(&editor, CTRL_A, (0, 0));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 0));
    step(&editor, "\x1b[1;5C", (2, 5));
    editor.handle_input(UP);
    step(&editor, UP, (0, 5));
}

#[test]
fn undo_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    press(&editor, UP, 2);
    line_start_then_right(&editor, 8);
    assert_eq!(cursor(&editor), (0, 8));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 8));
    step(&editor, "X", (2, 9));
    assert_eq!(editor.get_text(), "1234567890\n\n12345678X90");
    editor.handle_input(UP);
    step(&editor, UP, (0, 9));
    step(&editor, UNDO, (2, 8));
    assert_eq!(editor.get_text(), "1234567890\n\n1234567890");
    editor.handle_input(UP);
    step(&editor, UP, (0, 8));
}

#[test]
fn handles_consecutive_vertical_moves() {
    let editor = editor();
    editor.set_text("1234567890\nab\ncd\nef\n1234567890");
    line_start_then_right(&editor, 7);
    assert_eq!(cursor(&editor), (4, 7));
    press(&editor, UP, 3);
    step(&editor, UP, (0, 7));
    press(&editor, DOWN, 3);
    step(&editor, DOWN, (4, 7));
}

#[test]
fn moves_through_wrapped_visual_lines() {
    let editor = editor_cols(15);
    editor.set_text("short\n123456789012345678901234567890");
    editor.render(15);
    assert_eq!(cursor(&editor), (1, 30));
    editor.handle_input(UP);
    assert_eq!(cursor(&editor).0, 1);
    editor.handle_input(UP);
    assert_eq!(cursor(&editor).0, 1);
    editor.handle_input(UP);
    assert_eq!(cursor(&editor).0, 0);
}

#[test]
fn set_text_resets_the_column() {
    let editor = editor();
    editor.set_text("1234567890\n\n1234567890");
    line_start_then_right(&editor, 8);
    editor.handle_input(UP);
    editor.set_text("abcdefghij\n\nabcdefghij");
    assert_eq!(cursor(&editor), (2, 10));
    editor.handle_input(UP);
    step(&editor, UP, (0, 10));
}

#[test]
fn right_at_the_end_sets_the_column() {
    let editor = editor();
    editor.set_text("111111111x1111111111\n\n333333333_");
    press(&editor, UP, 2);
    step(&editor, CTRL_E, (0, 20));
    editor.handle_input(DOWN);
    step(&editor, DOWN, (2, 10));
    step(&editor, RIGHT, (2, 10));
    editor.handle_input(UP);
    step(&editor, UP, (0, 10));
}

#[test]
fn handles_resizes_with_the_column_on_the_same_line() {
    let editor = editor();
    editor.set_text("12345678901234567890\n\n12345678901234567890");
    line_start_then_right(&editor, 15);
    editor.handle_input(UP);
    step(&editor, UP, (0, 15));
    editor.render(12);
    press(&editor, DOWN, 2);
    assert_eq!(cursor(&editor).1, 4);
}

#[test]
fn handles_resizes_with_the_column_on_another_line() {
    let editor = editor();
    editor.set_text("short\n12345678901234567890");
    line_start_then_right(&editor, 15);
    assert_eq!(cursor(&editor), (1, 15));
    step(&editor, UP, (0, 5));
    editor.render(10);
    step(&editor, DOWN, (1, 8));
    step(&editor, UP, (0, 5));
    editor.render(80);
    step(&editor, DOWN, (1, 15));
}

#[test]
fn rewrapped_target_fits_the_current_column() {
    let editor = editor();
    editor.set_text("abcdefghijklmnopqr\n123456789012345678");
    position_cursor(&editor, 0, 18);
    assert_eq!(cursor(&editor), (0, 18));
    editor.render(10);
    step(&editor, DOWN, (1, 8));
    editor.render(80);
    step(&editor, UP, (0, 8));
    step(&editor, DOWN, (1, 8));
}

#[test]
fn rewrapped_target_shorter_than_the_current_column() {
    let editor = editor();
    editor.set_text("abcdefghijklmnopqr\n123456789012345678\nab");
    position_cursor(&editor, 0, 18);
    assert_eq!(cursor(&editor), (0, 18));
    editor.render(10);
    step(&editor, DOWN, (1, 8));
    editor.render(80);
    step(&editor, DOWN, (2, 2));
    step(&editor, UP, (1, 8));
}

#[test]
fn page_keys_move_by_a_page() {
    let editor = editor();
    editor.set_text(&(0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n"));
    editor.render(80);
    assert_eq!(cursor(&editor), (19, 7));
    step(&editor, "\x1b[5~", (12, 7));
    step(&editor, "\x1b[5~", (5, 6));
    step(&editor, "\x1b[6~", (12, 7));
}
