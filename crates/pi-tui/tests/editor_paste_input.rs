//! Pasted terminal output and bracketed pastes without an end marker (upstream issue 11383).

mod editor_common;

use editor_common::editor;
use pi_tui::stdin_buffer::PASTE_MAX_BYTES;

fn paste(content: &str) -> String {
    let editor = editor();
    editor.handle_input(&format!("\x1b[200~{content}\x1b[201~"));
    editor.get_text()
}

#[test]
fn strips_color_sequences_from_pasted_text() {
    assert_eq!(paste("\x1b[38;2;111;63;32m deploy done \u{2713}\x1b[0m"), " deploy done \u{2713}");
    assert_eq!(paste("\x1b[1;31merror\x1b[m: \x1b[4mbad\x1b[24m"), "error: bad");
}

#[test]
fn strips_mouse_reports_and_other_csi_sequences() {
    assert_eq!(paste("a\x1b[<35;153;17Mb\x1b[<0;1;2mc"), "abc");
    assert_eq!(paste("x\x1b[?25ly\x1b[2Jz\x1b[3 q"), "xyz");
}

#[test]
fn strips_osc_sequences_with_either_terminator() {
    assert_eq!(paste("\x1b]8;;https://example.com\x07link\x1b]8;;\x07"), "link");
    assert_eq!(paste("\x1b]0;title\x1b\\text"), "text");
}

#[test]
fn keeps_tmux_ctrl_letters_and_plain_brackets() {
    assert_eq!(paste("one\x1b[106;5utwo"), "one\ntwo");
    assert_eq!(paste("[38;2m stays [0m"), "[38;2m stays [0m");
}

#[test]
fn ends_a_paste_without_end_marker_at_the_size_cap() {
    let editor = editor();
    editor.handle_input("\x1b[200~");
    editor.handle_input(&"x".repeat(PASTE_MAX_BYTES));
    editor.handle_input("y");
    assert_eq!(editor.get_text(), format!("[paste #1 {} chars]", PASTE_MAX_BYTES + 1));
    editor.handle_input("z");
    assert_eq!(editor.get_text(), format!("[paste #1 {} chars]z", PASTE_MAX_BYTES + 1));
}

#[test]
fn keys_stay_buffered_while_a_paste_is_open() {
    let editor = editor();
    editor.handle_input("\x1b[200~abc");
    editor.handle_input("d");
    assert_eq!(editor.get_text(), "");
    editor.handle_input("\x1b[201~e");
    assert_eq!(editor.get_text(), "abcde");
}
