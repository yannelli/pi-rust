//! Port of upstream keys.test.ts: "Kitty protocol with alternate keys" and "modifyOtherKeys matching".

use std::sync::MutexGuard;

use pi_tui::env::global_state_lock;
use pi_tui::keys::{Key, matches_key, parse_key, set_kitty_protocol_active};

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
fn matches_ctrl_c_from_cyrillic_with_base_layout_key() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1089::99;5u", "ctrl+c"));
}

#[test]
fn matches_ctrl_d_from_cyrillic_with_base_layout_key() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1074::100;5u", "ctrl+d"));
}

#[test]
fn matches_ctrl_z_from_cyrillic_with_base_layout_key() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1103::122;5u", "ctrl+z"));
}

#[test]
fn matches_ctrl_shift_p_with_base_layout_key() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1079::112;6u", "ctrl+shift+p"));
}

#[test]
fn matches_direct_codepoint_without_base_layout_key() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[99;5u", "ctrl+c"));
}

#[test]
fn matches_super_bindings_including_combined_modifiers() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[107;9u", "super+k"));
    assert!(matches_key("\x1b[13;9u", "super+enter"));
    assert!(matches_key("\x1b[107;13u", &Key::ctrl_super("k")));
    assert!(matches_key("\x1b[107;13u", "ctrl+super+k"));
    assert!(matches_key("\x1b[107;14u", "ctrl+shift+super+k"));
    assert!(!matches_key("\x1b[107;13u", "super+k"));
    assert_eq!(parse_key("\x1b[107;9u").as_deref(), Some("super+k"));
    assert_eq!(parse_key("\x1b[13;9u").as_deref(), Some("super+enter"));
    assert_eq!(parse_key("\x1b[107;13u").as_deref(), Some("ctrl+super+k"));
    assert_eq!(parse_key("\x1b[107;14u").as_deref(), Some("shift+ctrl+super+k"));
}

#[test]
fn matches_digit_bindings_via_csi_u() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[49u", "1"));
    assert!(matches_key("\x1b[49;5u", "ctrl+1"));
    assert!(!matches_key("\x1b[49;5u", "ctrl+2"));
    assert_eq!(parse_key("\x1b[49u").as_deref(), Some("1"));
    assert_eq!(parse_key("\x1b[49;5u").as_deref(), Some("ctrl+1"));
}

#[test]
fn normalizes_keypad_functional_keys() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[57400u", "1"));
    assert!(matches_key("\x1b[57410u", "/"));
    assert!(matches_key("\x1b[57417u", "left"));
    assert!(matches_key("\x1b[57426u", "delete"));
    let cases = [
        ("\x1b[57399u", "0"),
        ("\x1b[57409u", "."),
        ("\x1b[57413u", "+"),
        ("\x1b[57416u", ","),
        ("\x1b[57417u", "left"),
        ("\x1b[57418u", "right"),
        ("\x1b[57419u", "up"),
        ("\x1b[57420u", "down"),
        ("\x1b[57421u", "pageUp"),
        ("\x1b[57422u", "pageDown"),
        ("\x1b[57423u", "home"),
        ("\x1b[57424u", "end"),
        ("\x1b[57425u", "insert"),
        ("\x1b[57426u", "delete"),
    ];
    for (data, expected) in cases {
        assert_eq!(parse_key(data).as_deref(), Some(expected), "{data:?}");
    }
}

#[test]
fn matches_shifted_symbols_by_produced_character() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[61:43;2u", "+"));
    assert!(matches_key("\x1b[61:43;2u", "shift+="));
    assert_eq!(parse_key("\x1b[61:43;2u").as_deref(), Some("+"));
    assert!(matches_key("\x1b[61:43;6u", "ctrl++"));
    assert_eq!(parse_key("\x1b[61:43;6u").as_deref(), Some("ctrl++"));
}

#[test]
fn handles_shifted_key_in_format() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[99:67:99;2u", "shift+c"));
}

#[test]
fn handles_event_type_in_format() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1089::99;5:3u", "ctrl+c"));
}

#[test]
fn handles_full_format_with_shifted_base_and_event() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[1089:1057:99;6:2u", "ctrl+shift+c"));
}

#[test]
fn prefers_codepoint_for_latin_letters_when_base_layout_differs() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[107::118;5u", "ctrl+k"));
    assert!(!matches_key("\x1b[107::118;5u", "ctrl+v"));
}

#[test]
fn prefers_codepoint_for_symbols_when_base_layout_differs() {
    let _state = kitty(true);
    assert!(matches_key("\x1b[47::91;5u", "ctrl+/"));
    assert!(!matches_key("\x1b[47::91;5u", "ctrl+["));
}

#[test]
fn does_not_match_wrong_key_with_base_layout() {
    let _state = kitty(true);
    assert!(!matches_key("\x1b[1089::99;5u", "ctrl+d"));
}

#[test]
fn does_not_match_wrong_modifiers_with_base_layout() {
    let _state = kitty(true);
    assert!(!matches_key("\x1b[1089::99;5u", "ctrl+shift+c"));
}

#[test]
fn modify_other_keys_ctrl_c() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;5;99~", "ctrl+c")]);
}

#[test]
fn modify_other_keys_ctrl_d() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;5;100~", "ctrl+d")]);
}

#[test]
fn modify_other_keys_ctrl_z() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;5;122~", "ctrl+z")]);
}

/// Every case asserts both `matches_key(data, key)` and `parse_key(data) == key`.
fn assert_round_trip(cases: &[(&str, &str)]) {
    for &(data, key) in cases {
        assert!(matches_key(data, key), "matches {data:?} {key}");
        assert_eq!(parse_key(data).as_deref(), Some(key), "parse {data:?}");
    }
}

#[test]
fn modify_other_keys_enter_variants() {
    let _state = kitty(false);
    assert_round_trip(&[
        ("\x1b[27;5;13~", "ctrl+enter"),
        ("\x1b[27;2;13~", "shift+enter"),
        ("\x1b[27;3;13~", "alt+enter"),
    ]);
}

#[test]
fn modify_other_keys_tab_variants() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;2;9~", "shift+tab"), ("\x1b[27;5;9~", "ctrl+tab"), ("\x1b[27;3;9~", "alt+tab")]);
}

#[test]
fn modify_other_keys_backspace_variants() {
    let _state = kitty(false);
    assert_round_trip(&[
        ("\x1b[27;1;127~", "backspace"),
        ("\x1b[27;5;127~", "ctrl+backspace"),
        ("\x1b[27;3;127~", "alt+backspace"),
    ]);
}

#[test]
fn modify_other_keys_escape() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;1;27~", "escape")]);
}

#[test]
fn modify_other_keys_space_variants() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;1;32~", "space"), ("\x1b[27;5;32~", "ctrl+space")]);
}

#[test]
fn modify_other_keys_symbol_combos() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;5;47~", "ctrl+/"), ("\x1b[27;6;43~", "ctrl++")]);
}

#[test]
fn modify_other_keys_digit_combos() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;5;49~", "ctrl+1"), ("\x1b[27;2;49~", "shift+1")]);
}

#[test]
fn modify_other_keys_shifted_uppercase_letters() {
    let _state = kitty(false);
    assert!(matches_key("\x1b[27;2;69~", "shift+e"));
    assert!(matches_key("\x1b[27;6;69~", "ctrl+shift+e"));
    assert_eq!(parse_key("\x1b[27;2;69~").as_deref(), Some("shift+e"));
    assert_eq!(parse_key("\x1b[27;6;69~").as_deref(), Some("shift+ctrl+e"));
}

#[test]
fn ctrl_alt_letter_via_csi_u_when_kitty_inactive() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[104;7u", "ctrl+alt+h")]);
}

#[test]
fn ctrl_alt_letter_via_modify_other_keys() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b[27;7;104~", "ctrl+alt+h")]);
}
