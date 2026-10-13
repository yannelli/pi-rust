//! Ports of the upstream editor undo tests.

mod editor_common;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use editor_common::{MockProvider, TestEditor, UNDO, cursor, editor, flush, suggestions, type_text};

const RIGHT: &str = "\x1b[C";
const CTRL_A: &str = "\x01";

fn move_right(editor: &TestEditor, times: usize) {
    editor.handle_input(CTRL_A);
    for _ in 0..times {
        editor.handle_input(RIGHT);
    }
}

#[test]
fn does_nothing_with_an_empty_stack() {
    let editor = editor();
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn coalesces_word_characters() {
    let editor = editor();
    type_text(&editor, "hello world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn undoes_spaces_one_at_a_time() {
    let editor = editor();
    type_text(&editor, "hello  ");
    for expected in ["hello ", "hello", ""] {
        editor.handle_input(UNDO);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn undoes_newlines_and_starts_a_new_word_unit() {
    let editor = editor();
    type_text(&editor, "hello\nworld");
    assert_eq!(editor.get_text(), "hello\nworld");
    for expected in ["hello\n", "hello", ""] {
        editor.handle_input(UNDO);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn undoes_backspace() {
    let editor = editor();
    type_text(&editor, "hello");
    editor.handle_input("\x7f");
    assert_eq!(editor.get_text(), "hell");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello");
}

#[test]
fn undoes_forward_delete() {
    let editor = editor();
    type_text(&editor, "hello");
    move_right(&editor, 1);
    editor.handle_input("\x1b[3~");
    assert_eq!(editor.get_text(), "hllo");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello");
}

#[test]
fn undoes_ctrl_w() {
    let editor = editor();
    type_text(&editor, "hello world");
    editor.handle_input("\x17");
    assert_eq!(editor.get_text(), "hello ");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn undoes_ctrl_k() {
    let editor = editor();
    type_text(&editor, "hello world");
    move_right(&editor, 6);
    editor.handle_input("\x0b");
    assert_eq!(editor.get_text(), "hello ");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
    editor.handle_input("|");
    assert_eq!(editor.get_text(), "hello |world");
}

#[test]
fn undoes_ctrl_u() {
    let editor = editor();
    type_text(&editor, "hello world");
    move_right(&editor, 6);
    editor.handle_input("\x15");
    assert_eq!(editor.get_text(), "world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn undoes_yank() {
    let editor = editor();
    type_text(&editor, "hello ");
    editor.handle_input("\x17");
    editor.handle_input("\x19");
    assert_eq!(editor.get_text(), "hello ");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn undoes_single_line_paste_at_once() {
    let editor = editor();
    editor.set_text("hello world");
    move_right(&editor, 5);
    editor.handle_input("\x1b[200~beep boop\x1b[201~");
    assert_eq!(editor.get_text(), "hellobeep boop world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
    editor.handle_input("|");
    assert_eq!(editor.get_text(), "hello| world");
}

#[test]
fn paste_does_not_trigger_autocomplete() {
    let editor = editor();
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    editor.set_autocomplete_provider(
        MockProvider::new(move |_, _, _, _| {
            counter.set(counter.get() + 1);
            None
        })
        .shared(),
    );
    editor.handle_input("\x1b[200~look at @node_modules/react/index.js please\x1b[201~");
    flush();
    assert_eq!(editor.get_text(), "look at @node_modules/react/index.js please");
    assert_eq!(calls.get(), 0);
    assert!(!editor.is_showing_autocomplete());
}

#[test]
fn decodes_csi_u_ctrl_letters_inside_paste() {
    let editor = editor();
    editor.handle_input("\x1b[200~line1\x1b[106;5uline2\x1b[106;5uline3\x1b[201~");
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
}

#[test]
fn undoes_multiline_paste_at_once() {
    let editor = editor();
    editor.set_text("hello world");
    move_right(&editor, 5);
    editor.handle_input("\x1b[200~line1\nline2\nline3\x1b[201~");
    assert_eq!(editor.get_text(), "helloline1\nline2\nline3 world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
    editor.handle_input("|");
    assert_eq!(editor.get_text(), "hello| world");
}

#[test]
fn undoes_insert_text_at_cursor_at_once() {
    let editor = editor();
    editor.set_text("hello world");
    move_right(&editor, 5);
    editor.insert_text_at_cursor("/tmp/image.png");
    assert_eq!(editor.get_text(), "hello/tmp/image.png world");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
    editor.handle_input("|");
    assert_eq!(editor.get_text(), "hello| world");
}

#[test]
fn insert_text_at_cursor_handles_multiline_text() {
    let editor = editor();
    editor.set_text("hello world");
    move_right(&editor, 5);
    editor.insert_text_at_cursor("line1\nline2\nline3");
    assert_eq!(editor.get_text(), "helloline1\nline2\nline3 world");
    assert_eq!(cursor(&editor), (2, 5));
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn insert_text_at_cursor_normalizes_line_endings() {
    let editor = editor();
    editor.set_text("");
    editor.insert_text_at_cursor("a\r\nb\r\nc");
    assert_eq!(editor.get_text(), "a\nb\nc");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
    editor.insert_text_at_cursor("x\ry\rz");
    assert_eq!(editor.get_text(), "x\ny\nz");
}

#[test]
fn undoes_set_text() {
    let editor = editor();
    type_text(&editor, "hello world");
    editor.set_text("");
    assert_eq!(editor.get_text(), "");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
}

#[test]
fn submit_clears_the_stack() {
    let editor = editor();
    let submitted = Rc::new(RefCell::new(String::new()));
    let sink = submitted.clone();
    editor.on_submit(move |text| *sink.borrow_mut() = text.to_string());
    type_text(&editor, "hello");
    editor.handle_input("\r");
    assert_eq!(*submitted.borrow(), "hello");
    assert_eq!(editor.get_text(), "");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn undo_exits_history_browsing() {
    let editor = editor();
    editor.add_to_history("hello");
    type_text(&editor, "world");
    editor.handle_input("\x17");
    assert_eq!(editor.get_text(), "");
    editor.handle_input("\x1b[A");
    assert_eq!(editor.get_text(), "hello");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "world");
}

#[test]
fn undo_restores_state_before_history_browsing() {
    let editor = editor();
    for entry in ["first", "second", "third"] {
        editor.add_to_history(entry);
    }
    type_text(&editor, "current");
    editor.handle_input("\x17");
    for expected in ["third", "second", "first"] {
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), expected);
    }
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "current");
}

#[test]
fn cursor_movement_starts_a_new_unit() {
    let editor = editor();
    type_text(&editor, "hello world");
    for _ in 0..5 {
        editor.handle_input("\x1b[D");
    }
    type_text(&editor, "lol");
    assert_eq!(editor.get_text(), "hello lolworld");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello world");
    editor.handle_input("|");
    assert_eq!(editor.get_text(), "hello |world");
}

#[test]
fn no_op_deletes_push_no_snapshots() {
    let editor = editor();
    type_text(&editor, "hello");
    for _ in 0..3 {
        editor.handle_input("\x17");
    }
    assert_eq!(editor.get_text(), "");
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "hello");
}

#[test]
fn undoes_autocomplete() {
    let editor = editor();
    editor.set_autocomplete_provider(
        MockProvider::new(
            |lines, _, col, _| {
                if &lines[0][..col] == "di" { suggestions(&["dist/"], "di") } else { None }
            },
        )
        .shared(),
    );
    type_text(&editor, "di");
    assert_eq!(editor.get_text(), "di");
    editor.handle_input("\t");
    flush();
    assert_eq!(editor.get_text(), "dist/");
    assert!(!editor.is_showing_autocomplete());
    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "di");
}
