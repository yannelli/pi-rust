//! Ports of the upstream editor character jump tests (Ctrl+] and Ctrl+Alt+]).

mod editor_common;

use editor_common::{UNDO, cursor, editor};

const JUMP: &str = "\x1d";
const JUMP_BACK: &str = "\x1b\x1d";
const CTRL_A: &str = "\x01";

#[test]
fn jumps_forward_on_the_same_line() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input(JUMP);
    editor.handle_input("o");
    assert_eq!(cursor(&editor), (0, 4));
}

#[test]
fn jumps_forward_past_the_cursor() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    for _ in 0..4 {
        editor.handle_input("\x1b[C");
    }
    assert_eq!(cursor(&editor), (0, 4));
    editor.handle_input(JUMP);
    editor.handle_input("o");
    assert_eq!(cursor(&editor), (0, 7));
}

#[test]
fn jumps_forward_across_lines() {
    let editor = editor();
    editor.set_text("abc\ndef\nghi");
    editor.handle_input("\x1b[A");
    editor.handle_input("\x1b[A");
    editor.handle_input(CTRL_A);
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input(JUMP);
    editor.handle_input("g");
    assert_eq!(cursor(&editor), (2, 0));
}

#[test]
fn jumps_backward_on_the_same_line() {
    let editor = editor();
    editor.set_text("hello world");
    assert_eq!(cursor(&editor), (0, 11));
    editor.handle_input(JUMP_BACK);
    editor.handle_input("o");
    assert_eq!(cursor(&editor), (0, 7));
}

#[test]
fn jumps_backward_across_lines() {
    let editor = editor();
    editor.set_text("abc\ndef\nghi");
    assert_eq!(cursor(&editor), (2, 3));
    editor.handle_input(JUMP_BACK);
    editor.handle_input("a");
    assert_eq!(cursor(&editor), (0, 0));
}

#[test]
fn stays_put_when_not_found() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input("z");
    assert_eq!(cursor(&editor), (0, 0));

    editor.set_text("hello world");
    assert_eq!(cursor(&editor), (0, 11));
    editor.handle_input(JUMP_BACK);
    editor.handle_input("z");
    assert_eq!(cursor(&editor), (0, 11));
}

#[test]
fn is_case_sensitive() {
    let editor = editor();
    editor.set_text("Hello World");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input("h");
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input(JUMP);
    editor.handle_input("W");
    assert_eq!(cursor(&editor), (0, 6));
}

#[test]
fn pressing_jump_again_cancels() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input(JUMP);
    editor.handle_input("o");
    assert_eq!(editor.get_text(), "ohello world");
}

#[test]
fn escape_cancels_and_is_processed() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input("\x1b");
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input("o");
    assert_eq!(editor.get_text(), "ohello world");
}

#[test]
fn pressing_backward_jump_again_cancels() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(JUMP_BACK);
    editor.handle_input(JUMP_BACK);
    editor.handle_input("o");
    assert_eq!(editor.get_text(), "hello worldo");
}

#[test]
fn finds_special_characters() {
    let editor = editor();
    editor.set_text("foo(bar) = baz;");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input("(");
    assert_eq!(cursor(&editor), (0, 3));
    editor.handle_input(JUMP);
    editor.handle_input("=");
    assert_eq!(cursor(&editor), (0, 9));
}

#[test]
fn handles_empty_text() {
    let editor = editor();
    editor.set_text("");
    editor.handle_input(JUMP);
    editor.handle_input("x");
    assert_eq!(cursor(&editor), (0, 0));
}

#[test]
fn jumping_resets_the_last_action() {
    let editor = editor();
    editor.set_text("hello world");
    editor.handle_input(CTRL_A);
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "xhello world");
    editor.handle_input(JUMP);
    editor.handle_input("o");
    editor.handle_input("Y");
    assert_eq!(editor.get_text(), "xhellYo world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "xhello world");
}

#[test]
fn jumps_over_multibyte_characters() {
    let editor = editor();
    editor.set_text("äöü ö");
    editor.handle_input(CTRL_A);
    editor.handle_input(JUMP);
    editor.handle_input("ö");
    assert_eq!(cursor(&editor), (0, "ä".len()));
    editor.handle_input(JUMP);
    editor.handle_input("ö");
    assert_eq!(cursor(&editor), (0, "äöü ".len()));
}
