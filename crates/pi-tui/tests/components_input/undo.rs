//! Upstream `Input component > Undo` cases.

use super::*;

#[test]
fn undo_does_nothing_when_stack_is_empty() {
    let (_lock, mut input) = setup();
    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "");
}

#[test]
fn undo_coalesces_consecutive_word_characters() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello world");
    assert_eq!(input.get_value(), "hello world");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello");
    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "");
}

#[test]
fn undo_removes_spaces_one_at_a_time() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello  ");
    assert_eq!(input.get_value(), "hello  ");
    for expected in ["hello ", "hello", ""] {
        input.handle_input(UNDO);
        assert_eq!(input.get_value(), expected);
    }
}

#[test]
fn undo_restores_backspace() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello");
    input.handle_input("\x7f");
    assert_eq!(input.get_value(), "hell");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello");
}

#[test]
fn undo_restores_forward_delete() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello");
    input.handle_input(CTRL_A);
    input.handle_input(RIGHT);
    input.handle_input("\x1b[3~");
    assert_eq!(input.get_value(), "hllo");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello");
}

#[test]
fn undo_restores_ctrl_w() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello world");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "hello ");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn undo_restores_ctrl_k() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);
    input.handle_input(CTRL_K);
    assert_eq!(input.get_value(), "hello ");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn undo_restores_ctrl_u() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);
    input.handle_input(CTRL_U);
    assert_eq!(input.get_value(), "world");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn undo_restores_yank() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "hello ");
    input.handle_input(CTRL_W);
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello ");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "");
}

#[test]
fn undo_restores_paste_atomically() {
    let (_lock, mut input) = setup();
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 5);

    input.handle_input("\x1b[200~beep boop\x1b[201~");
    assert_eq!(input.get_value(), "hellobeep boop world");

    // One undo restores the whole state from before the paste.
    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn undo_restores_alt_d() {
    let (_lock, mut input) = setup();
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    input.handle_input(ALT_D);
    assert_eq!(input.get_value(), " world");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn cursor_movement_starts_new_undo_unit() {
    let (_lock, mut input) = setup();
    type_text(&mut input, "abc");
    input.handle_input(CTRL_A);
    input.handle_input(CTRL_E);
    type_text(&mut input, "de");
    assert_eq!(input.get_value(), "abcde");

    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "abc");
    input.handle_input(UNDO);
    assert_eq!(input.get_value(), "");
}
