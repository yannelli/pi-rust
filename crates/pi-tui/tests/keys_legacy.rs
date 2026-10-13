//! Port of upstream keys.test.ts: "Legacy key matching" and parseKey "Legacy key parsing".

use std::sync::MutexGuard;

use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::keys::{matches_key, matches_key_with_env, parse_key, parse_key_with_env, set_kitty_protocol_active};

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

fn assert_round_trip(cases: &[(&str, &str)]) {
    for &(data, key) in cases {
        assert!(matches_key(data, key), "matches {data:?} {key}");
        assert_eq!(parse_key(data).as_deref(), Some(key), "parse {data:?}");
    }
}

#[test]
fn matches_legacy_ctrl_c() {
    let _state = kitty(false);
    assert!(matches_key("\x03", "ctrl+c"));
}

#[test]
fn matches_legacy_ctrl_d() {
    let _state = kitty(false);
    assert!(matches_key("\x04", "ctrl+d"));
}

#[test]
fn matches_escape_key() {
    let _state = kitty(false);
    assert!(matches_key("\x1b", "escape"));
}

#[test]
fn matches_legacy_linefeed_as_enter() {
    let _state = kitty(false);
    assert_round_trip(&[("\n", "enter")]);
}

#[test]
fn treats_linefeed_as_shift_enter_when_kitty_active() {
    let _state = kitty(true);
    assert!(matches_key("\n", "shift+enter"));
    assert!(!matches_key("\n", "enter"));
    assert_eq!(parse_key("\n").as_deref(), Some("shift+enter"));
}

#[test]
fn parses_ctrl_space() {
    let _state = kitty(false);
    assert_round_trip(&[("\x00", "ctrl+space")]);
}

#[test]
fn matches_legacy_ctrl_symbol() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1c", "ctrl+\\"), ("\x1d", "ctrl+]")]);
    // Ctrl+_ and Ctrl+- share a physical key on US keyboards and both send 0x1f.
    assert!(matches_key("\x1f", "ctrl+_"));
    assert_round_trip(&[("\x1f", "ctrl+-")]);
}

#[test]
fn matches_legacy_ctrl_alt_symbol() {
    let _state = kitty(false);
    assert_round_trip(&[("\x1b\x1b", "ctrl+alt+["), ("\x1b\x1c", "ctrl+alt+\\"), ("\x1b\x1d", "ctrl+alt+]")]);
    assert!(matches_key("\x1b\x1f", "ctrl+alt+_"));
    assert_round_trip(&[("\x1b\x1f", "ctrl+alt+-")]);
}

#[test]
fn treats_raw_0x08_as_plain_backspace_outside_windows_terminal() {
    let _state = kitty(false);
    let env = MapEnv::default();
    assert!(matches_key_with_env("\x7f", "backspace", &env));
    assert!(!matches_key_with_env("\x7f", "ctrl+backspace", &env));
    assert_eq!(parse_key_with_env("\x7f", &env).as_deref(), Some("backspace"));
    assert!(matches_key_with_env("\x08", "backspace", &env));
    assert!(!matches_key_with_env("\x08", "ctrl+backspace", &env));
    assert_eq!(parse_key_with_env("\x08", &env).as_deref(), Some("backspace"));
    assert!(matches_key_with_env("\x08", "ctrl+h", &env));
}

#[test]
fn treats_raw_0x08_as_ctrl_backspace_in_local_windows_terminal() {
    let _state = kitty(false);
    let env = MapEnv::new([("WT_SESSION", "test-session")]);
    assert!(matches_key_with_env("\x08", "ctrl+backspace", &env));
    assert!(!matches_key_with_env("\x08", "backspace", &env));
    assert_eq!(parse_key_with_env("\x08", &env).as_deref(), Some("ctrl+backspace"));
    assert!(matches_key_with_env("\x08", "ctrl+h", &env));
}

#[test]
fn treats_raw_0x08_as_plain_backspace_in_windows_terminal_over_ssh() {
    let _state = kitty(false);
    let env = MapEnv::new([
        ("WT_SESSION", "test-session"),
        ("SSH_CONNECTION", "1 2 3 4"),
        ("SSH_CLIENT", "1 2 3"),
        ("SSH_TTY", "/dev/pts/1"),
    ]);
    assert!(!matches_key_with_env("\x08", "ctrl+backspace", &env));
    assert!(matches_key_with_env("\x08", "backspace", &env));
    assert_eq!(parse_key_with_env("\x08", &env).as_deref(), Some("backspace"));
    assert!(matches_key_with_env("\x08", "ctrl+h", &env));
}

#[test]
fn parses_legacy_alt_prefixed_sequences_when_kitty_inactive() {
    let _state = kitty(false);
    assert_round_trip(&[
        ("\x1b ", "alt+space"),
        ("\x1b\x08", "alt+backspace"),
        ("\x1b\x03", "ctrl+alt+c"),
        ("\x1bB", "alt+left"),
        ("\x1bF", "alt+right"),
        ("\x1ba", "alt+a"),
        ("\x1b1", "alt+1"),
        ("\x1b,", "alt+,"),
        ("\x1b.", "alt+."),
        ("\x1by", "alt+y"),
        ("\x1bz", "alt+z"),
    ]);

    set_kitty_protocol_active(true);
    assert!(matches_key("\x1b\x08", "alt+backspace"));
    assert_eq!(parse_key("\x1b\x08").as_deref(), Some("alt+backspace"));
    let rejected = [
        ("\x1b ", "alt+space"),
        ("\x1b\x03", "ctrl+alt+c"),
        ("\x1bB", "alt+left"),
        ("\x1bF", "alt+right"),
        ("\x1ba", "alt+a"),
        ("\x1b1", "alt+1"),
        ("\x1b,", "alt+,"),
        ("\x1b.", "alt+."),
        ("\x1by", "alt+y"),
    ];
    for (data, key) in rejected {
        assert!(!matches_key(data, key), "matches {data:?} {key}");
        assert_eq!(parse_key(data), None, "parse {data:?}");
    }
}

#[test]
fn matches_arrow_keys() {
    let _state = kitty(false);
    assert!(matches_key("\x1b[A", "up"));
    assert!(matches_key("\x1b[B", "down"));
    assert!(matches_key("\x1b[C", "right"));
    assert!(matches_key("\x1b[D", "left"));
}

#[test]
fn matches_ss3_arrows_and_home_end() {
    let _state = kitty(false);
    assert!(matches_key("\x1bOA", "up"));
    assert!(matches_key("\x1bOB", "down"));
    assert!(matches_key("\x1bOC", "right"));
    assert!(matches_key("\x1bOD", "left"));
    assert!(matches_key("\x1bOH", "home"));
    assert!(matches_key("\x1bOF", "end"));
}

#[test]
fn matches_xterm_ctrl_modified_viewport_navigation() {
    let _state = kitty(false);
    assert_round_trip(&[
        ("\x1b[1;5H", "ctrl+home"),
        ("\x1b[1;5F", "ctrl+end"),
        ("\x1b[5;5~", "ctrl+pageUp"),
        ("\x1b[6;5~", "ctrl+pageDown"),
    ]);
}

#[test]
fn matches_legacy_function_keys_and_clear() {
    let _state = kitty(false);
    assert!(matches_key("\x1bOP", "f1"));
    assert!(matches_key("\x1b[24~", "f12"));
    assert!(matches_key("\x1b[E", "clear"));
}

#[test]
fn matches_alt_arrows() {
    let _state = kitty(false);
    assert!(matches_key("\x1bp", "alt+up"));
    assert!(!matches_key("\x1bp", "up"));
}

#[test]
fn matches_rxvt_modifier_sequences() {
    let _state = kitty(false);
    assert!(matches_key("\x1b[a", "shift+up"));
    assert!(matches_key("\x1bOa", "ctrl+up"));
    assert!(matches_key("\x1b[2$", "shift+insert"));
    assert!(matches_key("\x1b[2^", "ctrl+insert"));
    assert!(matches_key("\x1b[7$", "shift+home"));
}

#[test]
fn parses_legacy_ctrl_letter() {
    let _state = kitty(false);
    assert_eq!(parse_key("\x03").as_deref(), Some("ctrl+c"));
    assert_eq!(parse_key("\x04").as_deref(), Some("ctrl+d"));
}

#[test]
fn parses_special_keys() {
    let _state = kitty(false);
    let cases =
        [("\x1b", "escape"), ("\t", "tab"), ("\r", "enter"), ("\n", "enter"), ("\x00", "ctrl+space"), (" ", "space")];
    for (data, key) in cases {
        assert_eq!(parse_key(data).as_deref(), Some(key), "{data:?}");
    }
    assert_round_trip(&[("1", "1"), ("+", "+")]);
}

#[test]
fn parses_arrow_keys() {
    let _state = kitty(false);
    for (data, key) in [("\x1b[A", "up"), ("\x1b[B", "down"), ("\x1b[C", "right"), ("\x1b[D", "left")] {
        assert_eq!(parse_key(data).as_deref(), Some(key), "{data:?}");
    }
}

#[test]
fn parses_ss3_arrows_and_home_end() {
    let _state = kitty(false);
    let cases = [
        ("\x1bOA", "up"),
        ("\x1bOB", "down"),
        ("\x1bOC", "right"),
        ("\x1bOD", "left"),
        ("\x1bOH", "home"),
        ("\x1bOF", "end"),
    ];
    for (data, key) in cases {
        assert_eq!(parse_key(data).as_deref(), Some(key), "{data:?}");
    }
}

#[test]
fn parses_legacy_function_and_modifier_sequences() {
    let _state = kitty(false);
    let cases =
        [("\x1bOP", "f1"), ("\x1b[24~", "f12"), ("\x1b[E", "clear"), ("\x1b[2^", "ctrl+insert"), ("\x1bp", "alt+up")];
    for (data, key) in cases {
        assert_eq!(parse_key(data).as_deref(), Some(key), "{data:?}");
    }
}

#[test]
fn parses_double_bracket_page_up() {
    let _state = kitty(false);
    assert_eq!(parse_key("\x1b[[5~").as_deref(), Some("pageUp"));
}
