//! Function keys in the Kitty keyboard protocol and with modifiers (upstream issue 11345).

use std::sync::MutexGuard;

use pi_tui::env::global_state_lock;
use pi_tui::keys::{
    KeyEventType, decode_kitty_printable, decode_printable_key, is_key_release, is_key_repeat, kitty_event_type,
    matches_key, parse_key, set_kitty_protocol_active,
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

/// The `CSI number ~` key number of F1 to F12.
const TILDE_NUMBERS: [u32; 12] = [11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 23, 24];

fn assert_key(data: &str, key: &str) {
    assert!(matches_key(data, key), "{data:?} should match {key}");
    assert_eq!(parse_key(data).as_deref(), Some(key), "{data:?}");
}

#[test]
fn matches_kitty_letter_forms_of_f1_f2_and_f4() {
    let _state = kitty(true);
    assert_key("\x1b[P", "f1");
    assert_key("\x1b[Q", "f2");
    assert_key("\x1b[S", "f4");
    assert!(!matches_key("\x1b[P", "f2"));
    assert!(!matches_key("\x1b[Q", "ctrl+f2"));
}

#[test]
fn matches_f3_as_csi_13_tilde() {
    let _state = kitty(true);
    assert_key("\x1b[13~", "f3");
    assert_key("\x1b[13;5~", "ctrl+f3");
    // `CSI R` is the cursor position report.
    assert!(!matches_key("\x1b[R", "f3"));
    assert!(!matches_key("\x1b[1;5R", "ctrl+f3"));
}

#[test]
fn matches_modified_letter_forms() {
    let _state = kitty(true);
    assert_key("\x1b[1;5P", "ctrl+f1");
    assert_key("\x1b[1;2Q", "shift+f2");
    assert_key("\x1b[1;3S", "alt+f4");
    assert_key("\x1b[1;8P", "shift+ctrl+alt+f1");
    assert!(matches_key("\x1b[1;8P", "ctrl+alt+shift+f1"));
    assert!(!matches_key("\x1b[1;5P", "f1"));
    assert!(!matches_key("\x1b[1;5P", "shift+f1"));
}

#[test]
fn matches_modified_tilde_forms_of_every_function_key() {
    for active in [false, true] {
        let _state = kitty(active);
        for (index, number) in TILDE_NUMBERS.iter().enumerate() {
            let key = format!("f{}", index + 1);
            assert_key(&format!("\x1b[{number}~"), &key);
            assert_key(&format!("\x1b[{number};5~"), &format!("ctrl+{key}"));
            assert_key(&format!("\x1b[{number};2~"), &format!("shift+{key}"));
            assert_key(&format!("\x1b[{number};6~"), &format!("shift+ctrl+{key}"));
            assert!(!matches_key(&format!("\x1b[{number};5~"), &key));
        }
    }
}

#[test]
fn xterm_modifier_forms_match_without_kitty() {
    let _state = kitty(false);
    assert_key("\x1b[1;5P", "ctrl+f1");
    assert_key("\x1b[15;2~", "shift+f5");
    assert_key("\x1b[24;3~", "alt+f12");
    assert!(matches_key("\x1bOP", "f1"));
}

#[test]
fn ignores_lock_modifiers() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1;65P", "f1"));
    assert!(matches_key("\x1b[15;129~", "f5"));
    assert!(matches_key("\x1b[1;69Q", "ctrl+f2"));
}

// Upstream deb12d78e: Kitty functional codepoints (F13 to F35 and the CSI u codes of other
// functional keys) are neither keys pi names nor text.
#[test]
fn ignores_unmapped_kitty_functional_keys() {
    let _state = kitty(true);
    for data in ["\x1b[57376u", "\x1b[57398;2u", "\x1b[57364u"] {
        assert_eq!(decode_kitty_printable(data), None, "{data:?}");
        assert_eq!(decode_printable_key(data), None, "{data:?}");
        assert_eq!(parse_key(data), None, "{data:?}");
    }
    assert!(!matches_key("\x1b[57364u", "f1"));
    // Keypad keys still decode.
    assert_eq!(decode_kitty_printable("\x1b[57399u").as_deref(), Some("0"));
}

#[test]
fn matches_upstream_function_key_cases() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1;2S", "shift+f4"));
    assert!(matches_key("\x1b[15;5~", "ctrl+f5"));
    assert!(matches_key("\x1b[24;3~", "alt+f12"));
    assert!(matches_key("\x1b[15;1:2~", "f5"));
    assert_eq!(parse_key("\x1b[21;2~").as_deref(), Some("shift+f10"));
    assert_eq!(parse_key("\x1b[1;5R"), None);
    assert!(is_key_repeat("\x1b[1;1:2S"));
}

#[test]
fn reads_event_types_of_letter_forms() {
    let _state = kitty(true);
    assert!(is_key_release("\x1b[1;1:3P"));
    assert!(is_key_repeat("\x1b[1;5:2Q"));
    assert!(!is_key_release("\x1b[P"));
    assert_eq!(kitty_event_type("\x1b[1;1:3S"), Some(KeyEventType::Release));
    assert_eq!(kitty_event_type("\x1b[13;1:2~"), Some(KeyEventType::Repeat));
    assert!(matches_key("\x1b[1;1:3P", "f1"));
    assert!(matches_key("\x1b[15;5:1~", "ctrl+f5"));
}
