//! Port of upstream keys.test.ts: decodeKittyPrintable, decodePrintableKey, and parseKey with
//! Kitty alternate keys, plus checks for the Key helper, release/repeat, and key id parsing.

use std::sync::MutexGuard;

use pi_tui::env::global_state_lock;
use pi_tui::keys::{
    Key, KeyEventType, decode_kitty_printable, decode_printable_key, is_key_release, is_key_repeat,
    is_kitty_protocol_active, kitty_event_type, matches_key, parse_key, set_kitty_protocol_active,
};

/// Holds the global state lock with the Kitty flag set; resets the flag on drop.
struct KittyState {
    _guard: MutexGuard<'static, ()>,
}

fn kitty(active: bool) -> KittyState {
    let guard = global_state_lock();
    set_kitty_protocol_active(active);
    KittyState { _guard: guard }
}

impl Drop for KittyState {
    fn drop(&mut self) {
        set_kitty_protocol_active(false);
    }
}

#[test]
fn decodes_kitty_keypad_functional_keys_to_printable_characters() {
    let cases = [
        ("\x1b[57399u", "0"),
        ("\x1b[57400u", "1"),
        ("\x1b[57409u", "."),
        ("\x1b[57410u", "/"),
        ("\x1b[57411u", "*"),
        ("\x1b[57412u", "-"),
        ("\x1b[57413u", "+"),
        ("\x1b[57415u", "="),
        ("\x1b[57416u", ","),
    ];
    for (data, expected) in cases {
        assert_eq!(decode_kitty_printable(data).as_deref(), Some(expected), "{data:?}");
    }
    assert_eq!(decode_kitty_printable("\x1b[57417u"), None);
}

#[test]
fn decodes_printable_xterm_modify_other_keys_sequences() {
    assert_eq!(decode_printable_key("\x1b[27;2;69~").as_deref(), Some("E"));
    assert_eq!(decode_printable_key("\x1b[27;2;196~").as_deref(), Some("\u{c4}"));
    assert_eq!(decode_printable_key("\x1b[27;2;32~").as_deref(), Some(" "));
    assert_eq!(decode_printable_key("\x1b[27;2;13~"), None);
    assert_eq!(decode_printable_key("\x1b[27;6;69~"), None);
}

#[test]
fn parse_returns_latin_key_name_with_base_layout_key() {
    let _state = kitty(true);
    assert_eq!(parse_key("\x1b[1089::99;5u").as_deref(), Some("ctrl+c"));
}

#[test]
fn parse_prefers_codepoint_for_latin_letters_when_base_layout_differs() {
    let _state = kitty(true);
    assert_eq!(parse_key("\x1b[107::118;5u").as_deref(), Some("ctrl+k"));
}

#[test]
fn parse_prefers_codepoint_for_symbols_when_base_layout_differs() {
    let _state = kitty(true);
    assert_eq!(parse_key("\x1b[47::91;5u").as_deref(), Some("ctrl+/"));
}

#[test]
fn parse_returns_key_name_from_codepoint_without_base_layout() {
    let _state = kitty(true);
    assert_eq!(parse_key("\x1b[99;5u").as_deref(), Some("ctrl+c"));
}

#[test]
fn parses_shifted_uppercase_csi_u_letters_as_shift_letter() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[69;2u", "shift+e"));
    assert_eq!(parse_key("\x1b[69;2u").as_deref(), Some("shift+e"));
}

#[test]
fn ignores_kitty_csi_u_with_unsupported_modifiers() {
    let _state = kitty(true);
    assert_eq!(parse_key("\x1b[99;17u"), None);
}

#[test]
fn key_helper_builds_ids() {
    assert_eq!(Key::ESCAPE, "escape");
    assert_eq!(Key::PAGE_UP, "pageUp");
    assert_eq!(Key::BACKSLASH, "\\");
    assert_eq!(Key::ctrl("c"), "ctrl+c");
    assert_eq!(Key::super_("k"), "super+k");
    assert_eq!(Key::ctrl_shift("p"), "ctrl+shift+p");
    assert_eq!(Key::shift_ctrl("p"), "shift+ctrl+p");
    assert_eq!(Key::ctrl_shift_super(Key::ENTER), "ctrl+shift+super+enter");
    assert_eq!(Key::NAMED.len(), 61);
    assert!(Key::NAMED.contains(&"?"));
}

#[test]
fn detects_release_and_repeat_events() {
    assert!(is_key_release("\x1b[97;1:3u"));
    assert!(is_key_release("\x1b[1;1:3A"));
    assert!(is_key_release("\x1b[3;1:3~"));
    assert!(!is_key_release("\x1b[97u"));
    assert!(is_key_repeat("\x1b[97;1:2u"));
    assert!(is_key_repeat("\x1b[1;5:2H"));
    assert!(!is_key_repeat("\x1b[97;1:3u"));
    // Pasted text such as a MAC address is not a release or repeat.
    assert!(!is_key_release("\x1b[200~90:62:3F:A5\x1b[201~"));
    assert!(!is_key_repeat("\x1b[200~90:62:2F:A5\x1b[201~"));
    assert_eq!(kitty_event_type("\x1b[97;1:3u"), Some(KeyEventType::Release));
    assert_eq!(kitty_event_type("\x1b[97;1:2u"), Some(KeyEventType::Repeat));
    assert_eq!(kitty_event_type("\x1b[97u"), Some(KeyEventType::Press));
    assert_eq!(kitty_event_type("a"), None);
}

#[test]
fn rejects_invalid_key_ids_and_ignores_case() {
    let _state = kitty(false);
    assert!(!matches_key("\x03", "ctrl+ctrl+c"));
    assert!(!matches_key("\x03", "ctrl+"));
    assert!(!matches_key("a", "a+b"));
    assert!(!matches_key("a", ""));
    assert!(matches_key("\x03", "Ctrl+C"));
    assert!(matches_key("\x1b[5~", "PAGEUP"));
    assert!(matches_key("+", "+"));
    assert!(!matches_key("\x1b[15;2~", "shift+f5"));
}

#[test]
fn kitty_flag_is_visible_across_threads() {
    let _state = kitty(false);
    std::thread::spawn(|| set_kitty_protocol_active(true)).join().expect("thread runs");
    assert!(is_kitty_protocol_active());
    assert!(matches_key("\n", "shift+enter"));
}
