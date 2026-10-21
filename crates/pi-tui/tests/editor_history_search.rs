//! The reverse prompt history search (pi-rust extension, `tui.editor.historySearch`).

mod editor_common;

use std::cell::Cell;
use std::rc::Rc;

use editor_common::{TestEditor, UNDO, cursor, editor, type_text};
use pi_tui::utils::strip_terminal_sequences;

const SEARCH: &str = "\x12";
const ESCAPE: &str = "\x1b";
const ENTER: &str = "\r";
const BACKSPACE: &str = "\x7f";

fn with_history(entries: &[&str]) -> TestEditor {
    let editor = editor();
    for entry in entries {
        editor.add_to_history(entry);
    }
    editor
}

fn prompt_line(editor: &TestEditor) -> String {
    let lines = editor.render(80).iter().map(|line| strip_terminal_sequences(line)).collect::<Vec<_>>();
    lines.into_iter().find(|line| line.contains("reverse-i-search")).unwrap_or_default()
}

#[test]
fn filters_case_insensitively_and_shows_the_newest_match() {
    let editor = with_history(&["deploy the app", "run the tests", "Deploy staging"]);
    editor.set_text("draft");
    editor.handle_input(SEARCH);
    assert!(editor.is_searching_history());
    assert!(prompt_line(&editor).contains("(reverse-i-search)'': "), "{}", prompt_line(&editor));
    assert_eq!(editor.get_text(), "draft");

    type_text(&editor, "DEPLOY");
    assert_eq!(editor.get_text(), "Deploy staging");
    assert_eq!(cursor(&editor), (0, 0));
    assert!(prompt_line(&editor).contains("'DEPLOY': Deploy staging"), "{}", prompt_line(&editor));

    type_text(&editor, " T");
    assert_eq!(editor.get_text(), "deploy the app");
}

#[test]
fn search_key_moves_to_older_matches_and_skips_repeated_prompts() {
    let editor = with_history(&["fix the parser", "fix the lexer", "unrelated", "fix the lexer"]);
    editor.handle_input(SEARCH);
    type_text(&editor, "fix");
    assert_eq!(editor.get_text(), "fix the lexer");

    editor.handle_input(SEARCH);
    assert_eq!(editor.get_text(), "fix the parser");

    editor.handle_input(SEARCH);
    assert_eq!(editor.get_text(), "fix the parser");
    assert!(prompt_line(&editor).contains("(failed reverse-i-search)'fix'"), "{}", prompt_line(&editor));
}

#[test]
fn places_the_cursor_on_the_match_in_a_multiline_entry() {
    let editor = with_history(&["first line\nsecond needle here"]);
    editor.handle_input(SEARCH);
    type_text(&editor, "needle");
    assert_eq!(cursor(&editor), (1, 7));
    assert!(prompt_line(&editor).contains("'needle': second needle here"), "{}", prompt_line(&editor));
}

#[test]
fn backspace_widens_the_query_again() {
    let editor = with_history(&["alpha", "beta"]);
    editor.handle_input(SEARCH);
    type_text(&editor, "alx");
    assert_eq!(editor.get_text(), "alpha");
    assert!(prompt_line(&editor).contains("failed"));
    editor.handle_input(BACKSPACE);
    assert!(!prompt_line(&editor).contains("failed"));
    editor.handle_input(BACKSPACE);
    editor.handle_input(BACKSPACE);
    assert_eq!(editor.get_text(), "", "an empty query shows the draft again");
}

#[test]
fn enter_accepts_the_match_without_submitting() {
    let editor = with_history(&["run the tests"]);
    let submitted = Rc::new(Cell::new(false));
    let flag = submitted.clone();
    editor.on_submit(move |_| flag.set(true));
    editor.set_text("draft");
    editor.handle_input(SEARCH);
    type_text(&editor, "tests");
    editor.handle_input(ENTER);
    assert!(!editor.is_searching_history());
    assert!(!submitted.get());
    assert_eq!(editor.get_text(), "run the tests");
    assert!(prompt_line(&editor).is_empty());

    editor.handle_input(UNDO);
    assert_eq!(editor.get_text(), "draft", "the draft is one undo step back");
}

#[test]
fn escape_cancels_and_restores_the_draft() {
    let editor = with_history(&["run the tests"]);
    editor.set_text("my draft");
    editor.handle_input("\x1b[D");
    editor.handle_input(SEARCH);
    type_text(&editor, "run");
    assert_eq!(editor.get_text(), "run the tests");
    editor.handle_input(ESCAPE);
    assert!(!editor.is_searching_history());
    assert_eq!(editor.get_text(), "my draft");
    assert_eq!(cursor(&editor), (0, 7));
}

#[test]
fn other_keys_accept_and_then_act() {
    let editor = with_history(&["run the tests"]);
    editor.handle_input(SEARCH);
    type_text(&editor, "the");
    assert_eq!(cursor(&editor), (0, 4));
    editor.handle_input("\x05");
    assert!(!editor.is_searching_history());
    assert_eq!(editor.get_text(), "run the tests");
    assert_eq!(cursor(&editor), (0, 13));
}

#[test]
fn paste_extends_the_query() {
    let editor = with_history(&["deploy staging", "deploy production"]);
    editor.handle_input(SEARCH);
    editor.handle_input("\x1b[200~stag\x1b[201~");
    assert_eq!(editor.get_text(), "deploy staging");
    assert!(editor.is_searching_history());
}

#[test]
fn history_limit_keeps_the_newest_entries() {
    let editor = with_history(&["one", "two", "three"]);
    editor.set_history_limit(2);
    for expected in ["three", "two", "two"] {
        editor.handle_input("\x1b[A");
        assert_eq!(editor.get_text(), expected);
    }
}

#[test]
fn set_text_closes_the_search() {
    let editor = with_history(&["run the tests"]);
    editor.handle_input(SEARCH);
    type_text(&editor, "run");
    editor.set_text("replaced");
    assert!(!editor.is_searching_history());
    editor.handle_input(ESCAPE);
    assert_eq!(editor.get_text(), "replaced");
}
