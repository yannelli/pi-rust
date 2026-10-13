//! Ports of the upstream editor tests: prompt history, state accessors, backslash newline, and
//! Kitty CSI-u input.

mod editor_common;

use std::cell::Cell;
use std::rc::Rc;

use editor_common::{cursor, editor};

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const LEFT: &str = "\x1b[D";

#[test]
fn does_nothing_on_up_when_history_is_empty() {
    let editor = editor();
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "");
}

#[test]
fn shows_most_recent_entry_on_up_when_empty() {
    let editor = editor();
    editor.add_to_history("first prompt");
    editor.add_to_history("second prompt");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "second prompt");
}

#[test]
fn cycles_through_entries_on_repeated_up() {
    let editor = editor();
    for entry in ["first", "second", "third"] {
        editor.add_to_history(entry);
    }
    for expected in ["third", "second", "first", "first"] {
        editor.handle_input(UP);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn jumps_to_start_before_entering_history_from_a_draft() {
    let editor = editor();
    editor.add_to_history("prompt");
    editor.set_text("draft");
    editor.handle_input(LEFT);
    editor.handle_input(LEFT);

    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "draft");
    assert_eq!(cursor(&editor), (0, 0));

    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "prompt");

    editor.handle_input(DOWN);
    assert_eq!(editor.get_text(), "draft");
    assert_eq!(cursor(&editor), (0, 0));
}

#[test]
fn navigates_forward_with_down() {
    let editor = editor();
    for entry in ["first", "second", "third"] {
        editor.add_to_history(entry);
    }
    editor.set_text("draft");
    for _ in 0..4 {
        editor.handle_input(UP);
    }
    for expected in ["second", "third", "draft"] {
        editor.handle_input(DOWN);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn exits_history_mode_when_typing() {
    let editor = editor();
    editor.add_to_history("old prompt");
    editor.handle_input(UP);
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "xold prompt");
}

#[test]
fn exits_history_mode_on_set_text() {
    let editor = editor();
    editor.add_to_history("first");
    editor.add_to_history("second");
    editor.handle_input(UP);
    editor.set_text("");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "second");
}

#[test]
fn skips_empty_entries() {
    let editor = editor();
    editor.add_to_history("");
    editor.add_to_history("   ");
    editor.add_to_history("valid");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "valid");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "valid");
}

#[test]
fn skips_consecutive_duplicates() {
    let editor = editor();
    for _ in 0..3 {
        editor.add_to_history("same");
    }
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "same");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "same");
}

#[test]
fn allows_non_consecutive_duplicates() {
    let editor = editor();
    for entry in ["first", "second", "first"] {
        editor.add_to_history(entry);
    }
    for expected in ["first", "second", "first"] {
        editor.handle_input(UP);
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn moves_the_cursor_instead_of_history_with_content() {
    let editor = editor();
    editor.add_to_history("history item");
    editor.set_text("line1\nline2");
    editor.handle_input(UP);
    editor.handle_input("X");
    assert_eq!(editor.get_text(), "line1X\nline2");
}

#[test]
fn keeps_at_most_100_entries() {
    let editor = editor();
    for i in 0..105 {
        editor.add_to_history(&format!("prompt {i}"));
    }
    for _ in 0..100 {
        editor.handle_input(UP);
    }
    assert_eq!(editor.get_text(), "prompt 5");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "prompt 5");
}

#[test]
fn places_cursor_at_start_when_browsing_up() {
    let editor = editor();
    editor.add_to_history("older entry");
    editor.add_to_history("line1\nline2\nline3");
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "older entry");
    assert_eq!(cursor(&editor), (0, 0));
}

#[test]
fn places_cursor_at_end_when_browsing_down() {
    let editor = editor();
    editor.add_to_history("older entry");
    editor.add_to_history("line1\nline2\nline3");
    editor.add_to_history("newer entry");
    for _ in 0..3 {
        editor.handle_input(UP);
    }
    editor.handle_input(DOWN);
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
    assert_eq!(cursor(&editor), (2, 5));
    editor.handle_input(DOWN);
    assert_eq!(editor.get_text(), "newer entry");
}

#[test]
fn allows_opposite_direction_movement_in_multiline_entry() {
    let editor = editor();
    editor.add_to_history("line1\nline2\nline3");
    editor.handle_input(UP);
    assert_eq!(cursor(&editor), (0, 0));
    editor.handle_input(DOWN);
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
    assert_eq!(cursor(&editor), (1, 0));
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "line1\nline2\nline3");
    assert_eq!(cursor(&editor), (0, 0));
}

#[test]
fn returns_cursor_position() {
    let editor = editor();
    assert_eq!(cursor(&editor), (0, 0));
    for c in ["a", "b", "c"] {
        editor.handle_input(c);
    }
    assert_eq!(cursor(&editor), (0, 3));
    editor.handle_input(LEFT);
    assert_eq!(cursor(&editor), (0, 2));
}

#[test]
fn returns_lines_as_a_copy() {
    let editor = editor();
    editor.set_text("a\nb");
    let mut lines = editor.get_lines();
    assert_eq!(lines, ["a", "b"]);
    lines[0] = "mutated".to_string();
    assert_eq!(editor.get_lines(), ["a", "b"]);
}

#[test]
fn inserts_backslash_immediately() {
    let editor = editor();
    editor.handle_input("\\");
    assert_eq!(editor.get_text(), "\\");
}

#[test]
fn converts_standalone_backslash_to_newline_on_enter() {
    let editor = editor();
    editor.handle_input("\\");
    editor.handle_input("\r");
    assert_eq!(editor.get_text(), "\n");
}

#[test]
fn inserts_backslash_normally_before_other_characters() {
    let editor = editor();
    editor.handle_input("\\");
    editor.handle_input("x");
    assert_eq!(editor.get_text(), "\\x");
}

#[test]
fn submits_when_backslash_is_not_before_the_cursor() {
    let editor = editor();
    let submitted = Rc::new(Cell::new(false));
    let flag = submitted.clone();
    editor.on_submit(move |_| flag.set(true));
    editor.handle_input("\\");
    editor.handle_input("x");
    editor.handle_input("\r");
    assert!(submitted.get());
}

#[test]
fn removes_only_one_backslash() {
    let editor = editor();
    for _ in 0..3 {
        editor.handle_input("\\");
    }
    assert_eq!(editor.get_text(), "\\\\\\");
    editor.handle_input("\r");
    assert_eq!(editor.get_text(), "\\\\\n");
}

#[test]
fn ignores_csi_u_with_unsupported_modifiers() {
    let editor = editor();
    editor.handle_input("\x1b[99;9u");
    assert_eq!(editor.get_text(), "");
}

#[test]
fn inserts_shifted_csi_u_letters() {
    let editor = editor();
    editor.handle_input("\x1b[69;2u");
    assert_eq!(editor.get_text(), "E");
}

#[test]
fn inserts_shifted_modify_other_keys_letters() {
    let editor = editor();
    editor.handle_input("\x1b[27;2;69~");
    assert_eq!(editor.get_text(), "E");
}

#[test]
fn submit_and_change_callbacks_can_use_the_editor() {
    let editor = editor();
    let handle = editor.editor.clone();
    let changes = Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = changes.clone();
    editor.on_change(move |text| seen.borrow_mut().push(text.to_string()));
    editor.on_submit(move |text| handle.add_to_history(text));
    editor.handle_input("h");
    editor.handle_input("i");
    editor.handle_input("\r");
    assert_eq!(*changes.borrow(), ["h", "hi", ""]);
    editor.handle_input(UP);
    assert_eq!(editor.get_text(), "hi");
}
