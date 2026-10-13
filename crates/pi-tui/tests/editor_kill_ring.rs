//! Ports of the upstream editor kill ring tests (Ctrl+W/U/K, Alt+D, Ctrl+Y, Alt+Y).

mod editor_common;

use editor_common::{TestEditor, editor};

const CTRL_W: &str = "\x17";
const CTRL_U: &str = "\x15";
const CTRL_K: &str = "\x0b";
const CTRL_Y: &str = "\x19";
const ALT_Y: &str = "\x1by";
const ALT_D: &str = "\x1bd";
const CTRL_A: &str = "\x01";
const CTRL_E: &str = "\x05";
const RIGHT: &str = "\x1b[C";
const UP: &str = "\x1b[A";

fn press(editor: &TestEditor, key: &str, times: usize) {
    for _ in 0..times {
        editor.handle_input(key);
    }
}

/// Kills each text with Ctrl+W in turn, leaving one ring entry per text.
fn fill_ring(editor: &TestEditor, texts: &[&str]) {
    for text in texts {
        editor.set_text(text);
        editor.handle_input(CTRL_W);
    }
}

#[test]
fn ctrl_w_kills_and_ctrl_y_yanks() {
    let editor = editor();
    editor.set_text("foo bar baz");
    editor.handle_input(CTRL_W);
    assert_eq!(editor.get_text(), "foo bar ");
    editor.handle_input(CTRL_A);
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "bazfoo bar ");
}

#[test]
fn ctrl_u_kills_to_line_start() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    press(&editor, RIGHT, 6);
    editor.handle_input(CTRL_U);
    assert_eq!(editor.get_text(), "world");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn ctrl_k_kills_to_line_end() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    editor.handle_input(CTRL_K);
    assert_eq!(editor.get_text(), "");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn ctrl_y_does_nothing_with_empty_ring() {
    let editor = editor();
    editor.set_text("test");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "test");
}

#[test]
fn alt_y_cycles_after_ctrl_y() {
    let editor = editor();
    fill_ring(&editor, &["first", "second", "third"]);
    assert_eq!(editor.get_text(), "");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "third");
    for expected in ["second", "first", "third"] {
        editor.handle_input(ALT_Y);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn alt_y_needs_a_preceding_yank() {
    let editor = editor();
    fill_ring(&editor, &["test"]);
    editor.set_text("other");
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "otherx");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "otherx");
}

#[test]
fn alt_y_needs_two_entries() {
    let editor = editor();
    fill_ring(&editor, &["only"]);
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "only");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "only");
}

#[test]
fn consecutive_ctrl_w_accumulates() {
    let editor = editor();
    editor.set_text("one two three");
    press(&editor, CTRL_W, 3);
    assert_eq!(editor.get_text(), "");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "one two three");
}

#[test]
fn ctrl_u_accumulates_multiline_deletes() {
    let editor = editor();
    editor.set_text("line1\nline2\nline3");
    for expected in ["line1\nline2\n", "line1\nline2", "line1\n", "line1", ""] {
        editor.handle_input(CTRL_U);
        assert_eq!(editor.get_text(), expected);
    }
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
}

#[test]
fn backward_kills_prepend_and_forward_kills_append() {
    let editor = editor();
    editor.set_text("prefix|suffix");
    editor.handle_input(CTRL_A);
    press(&editor, RIGHT, 6);
    press(&editor, CTRL_K, 2);
    assert_eq!(editor.get_text(), "prefix");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "prefix|suffix");
}

#[test]
fn other_actions_break_kill_accumulation() {
    let editor = editor();
    editor.set_text("foo bar baz");
    editor.handle_input(CTRL_W);
    assert_eq!(editor.get_text(), "foo bar ");
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "foo bar x");
    editor.handle_input(CTRL_W);
    assert_eq!(editor.get_text(), "foo bar ");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "foo bar x");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "foo bar baz");
}

#[test]
fn other_actions_break_the_yank_chain() {
    let editor = editor();
    fill_ring(&editor, &["first", "second"]);
    editor.set_text("");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "second");
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "secondx");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "secondx");
}

#[test]
fn ring_rotation_persists() {
    let editor = editor();
    fill_ring(&editor, &["first", "second", "third"]);
    editor.set_text("");
    editor.handle_input(CTRL_Y);
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "second");
    editor.handle_input("x");
    editor.set_text("");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "second");
}

#[test]
fn deletions_across_lines_coalesce() {
    let editor = editor();
    editor.set_text("1\n2\n3");
    for expected in ["1\n2\n", "1\n2", "1\n", "1", ""] {
        editor.handle_input(CTRL_W);
        assert_eq!(editor.get_text(), expected);
    }
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "1\n2\n3");
}

#[test]
fn ctrl_k_at_line_end_kills_the_newline() {
    let editor = editor();
    editor.set_text("");
    for key in ["a", "b", "\n", "c", "d", UP, CTRL_E] {
        editor.handle_input(key);
    }
    editor.handle_input(CTRL_K);
    assert_eq!(editor.get_text(), "abcd");
    editor.handle_input(CTRL_K);
    assert_eq!(editor.get_text(), "ab");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "ab\ncd");
}

#[test]
fn yanks_in_the_middle_of_text() {
    let editor = editor();
    fill_ring(&editor, &["word"]);
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    press(&editor, RIGHT, 6);
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello wordworld");
}

#[test]
fn yank_pops_in_the_middle_of_text() {
    let editor = editor();
    fill_ring(&editor, &["FIRST", "SECOND"]);
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    press(&editor, RIGHT, 6);
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello SECONDworld");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "hello FIRSTworld");
}

#[test]
fn multiline_yank_and_yank_pop_in_the_middle() {
    let editor = editor();
    fill_ring(&editor, &["SINGLE"]);
    editor.set_text("A\nB");
    press(&editor, CTRL_U, 3);
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    press(&editor, RIGHT, 6);
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello A\nBworld");
    editor.handle_input(ALT_Y);
    assert_eq!(editor.get_text(), "hello SINGLEworld");
}

#[test]
fn alt_d_kills_words_forward() {
    let editor = editor();
    editor.set_text("hello world test");
    editor.handle_input(CTRL_A);
    editor.handle_input(ALT_D);
    assert_eq!(editor.get_text(), " world test");
    editor.handle_input(ALT_D);
    assert_eq!(editor.get_text(), " test");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "hello world test");
}

#[test]
fn alt_d_at_line_end_kills_the_newline() {
    let editor = editor();
    editor.set_text("line1\nline2");
    editor.handle_input(UP);
    editor.handle_input(CTRL_E);
    editor.handle_input(ALT_D);
    assert_eq!(editor.get_text(), "line1line2");
    editor.handle_input(CTRL_Y);
    assert_eq!(editor.get_text(), "line1\nline2");
}
