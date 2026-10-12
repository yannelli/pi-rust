//! Upstream `Input component > Kill ring` cases.

use super::*;

/// Kills each word in turn, leaving the kill ring with one entry per word.
fn kill_each(input: &mut Input, words: &[&str]) {
    for word in words {
        value_with_cursor_at_end(input, word);
        input.handle_input(CTRL_W);
    }
}

#[test]
fn ctrl_w_saves_deleted_text_and_ctrl_y_yanks_it() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "foo bar baz");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "foo bar ");

    input.handle_input(CTRL_A);
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "bazfoo bar ");
}

#[test]
fn ctrl_w_preserves_ascii_punctuation_boundaries() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "foo.bar");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "foo.");

    value_with_cursor_at_end(&mut input, "foo:bar");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "foo:");
}

#[test]
fn ctrl_w_handles_unicode_word_boundaries() {
    // Upstream ICU groups "你好" and "世界"; UAX #29 without a dictionary stops at every
    // ideograph, so each ideograph is its own step. Punctuation stops match upstream.
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "你好世界。你好，世界");
    let head = ["你好世界。你好，世", "你好世界。你好，", "你好世界。你好", "你好世界。你", "你好世界。"];
    let tail = ["你好世界", "你好世", "你好", "你", ""];
    for expected in head.into_iter().chain(tail) {
        input.handle_input(CTRL_W);
        assert_eq!(input.get_value(), expected);
    }
}

#[test]
fn ctrl_u_saves_deleted_text_to_kill_ring() {
    let (_lock, mut input) = setup();
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);

    input.handle_input(CTRL_U);
    assert_eq!(input.get_value(), "world");
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn ctrl_k_saves_deleted_text_to_kill_ring() {
    let (_lock, mut input) = setup();
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    input.handle_input(CTRL_K);
    assert_eq!(input.get_value(), "");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello world");
}

#[test]
fn ctrl_y_does_nothing_when_kill_ring_is_empty() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "test");
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "test");
}

#[test]
fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["first", "second", "third"]);
    assert_eq!(input.get_value(), "");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "third");
    for expected in ["second", "first", "third"] {
        input.handle_input(ALT_Y);
        assert_eq!(input.get_value(), expected);
    }
}

#[test]
fn alt_y_does_nothing_if_not_preceded_by_yank() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["test"]);
    value_with_cursor_at_end(&mut input, "other");

    input.handle_input("x");
    assert_eq!(input.get_value(), "otherx");
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "otherx");
}

#[test]
fn alt_y_does_nothing_if_kill_ring_has_one_entry() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["only"]);

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "only");
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "only");
}

#[test]
fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "one two three");
    press(&mut input, CTRL_W, 3);
    assert_eq!(input.get_value(), "");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "one two three");
}

#[test]
fn non_delete_actions_break_kill_accumulation() {
    let (_lock, mut input) = setup();
    value_with_cursor_at_end(&mut input, "foo bar baz");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "foo bar ");

    input.handle_input("x");
    assert_eq!(input.get_value(), "foo bar x");
    input.handle_input(CTRL_W);
    assert_eq!(input.get_value(), "foo bar ");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "foo bar x");
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "foo bar baz");
}

#[test]
fn non_yank_actions_break_alt_y_chain() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["first", "second"]);
    input.set_value("");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "second");
    input.handle_input("x");
    assert_eq!(input.get_value(), "secondx");
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "secondx");
}

#[test]
fn kill_ring_rotation_persists_after_cycling() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["first", "second", "third"]);
    input.set_value("");

    input.handle_input(CTRL_Y);
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "second");

    input.handle_input("x");
    input.set_value("");
    // "second" is the newest entry after the rotation.
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "second");
}

#[test]
fn backward_deletions_prepend_forward_deletions_append() {
    let (_lock, mut input) = setup();
    input.set_value("prefix|suffix");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);

    input.handle_input(CTRL_K);
    assert_eq!(input.get_value(), "prefix");
    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "prefix|suffix");
}

#[test]
fn alt_d_deletes_word_forward_and_saves_to_kill_ring() {
    let (_lock, mut input) = setup();
    input.set_value("hello world test");
    input.handle_input(CTRL_A);

    input.handle_input(ALT_D);
    assert_eq!(input.get_value(), " world test");
    input.handle_input(ALT_D);
    assert_eq!(input.get_value(), " test");

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello world test");
}

#[test]
fn alt_d_preserves_ascii_punctuation_boundaries() {
    let (_lock, mut input) = setup();
    input.set_value("foo.bar baz");
    input.handle_input(CTRL_A);
    for expected in [".bar baz", "bar baz", " baz"] {
        input.handle_input(ALT_D);
        assert_eq!(input.get_value(), expected);
    }
}

#[test]
fn alt_d_handles_unicode_word_boundaries() {
    // Same segmentation difference as the Ctrl+W case: one step per ideograph.
    let (_lock, mut input) = setup();
    input.set_value("你好世界。你好，世界");
    input.handle_input(CTRL_A);
    let head = ["好世界。你好，世界", "世界。你好，世界", "界。你好，世界", "。你好，世界", "你好，世界"];
    let tail = ["好，世界", "，世界", "世界", "界", ""];
    for expected in head.into_iter().chain(tail) {
        input.handle_input(ALT_D);
        assert_eq!(input.get_value(), expected);
    }
}

#[test]
fn yank_in_middle_of_text() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["word"]);
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello wordworld");
}

#[test]
fn yank_pop_in_middle_of_text() {
    let (_lock, mut input) = setup();
    kill_each(&mut input, &["FIRST", "SECOND"]);
    input.set_value("hello world");
    input.handle_input(CTRL_A);
    press(&mut input, RIGHT, 6);

    input.handle_input(CTRL_Y);
    assert_eq!(input.get_value(), "hello SECONDworld");
    input.handle_input(ALT_Y);
    assert_eq!(input.get_value(), "hello FIRSTworld");
}
