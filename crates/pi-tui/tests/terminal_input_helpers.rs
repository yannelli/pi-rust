//! Port of the pure helper cases in upstream `test/terminal.test.ts`: `resolveEscapeTimeoutMs`,
//! `normalizeNativeShiftEnterInput`, and `normalizeAppleTerminalInput`.

use pi_tui::env::MapEnv;
use pi_tui::terminal::{normalize_apple_terminal_input, normalize_native_shift_enter_input, resolve_escape_timeout_ms};

fn escape_timeout(pairs: &[(&str, &str)]) -> u64 {
    resolve_escape_timeout_ms(&MapEnv::new(pairs.iter().copied()))
}

// resolveEscapeTimeoutMs

#[test]
fn uses_pi_tui_esc_timeout_when_configured() {
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "80")]), 80);
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "80"), ("SSH_TTY", "/dev/pts/1")]), 80);
}

#[test]
fn ignores_invalid_pi_tui_esc_timeout_values() {
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "abc")]), 10);
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "0")]), 10);
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "-5")]), 10);
    assert_eq!(escape_timeout(&[("PI_TUI_ESC_TIMEOUT", "")]), 10);
}

#[test]
fn defaults_to_100ms_over_ssh() {
    assert_eq!(escape_timeout(&[("SSH_CONNECTION", "10.0.0.1 22")]), 100);
    assert_eq!(escape_timeout(&[("SSH_TTY", "/dev/pts/1")]), 100);
}

#[test]
fn defaults_to_10ms_otherwise() {
    assert_eq!(escape_timeout(&[]), 10);
}

// normalizeNativeShiftEnterInput

#[test]
fn rewrites_return_to_csi_u_shift_enter_when_native_shift_detection_is_enabled_and_shift_is_pressed() {
    assert_eq!(normalize_native_shift_enter_input("\r", true, true), "\x1b[13;2u");
}

#[test]
fn leaves_return_unchanged_when_native_shift_detection_is_disabled() {
    assert_eq!(normalize_native_shift_enter_input("\r", false, true), "\r");
}

#[test]
fn leaves_return_unchanged_when_shift_is_not_pressed() {
    assert_eq!(normalize_native_shift_enter_input("\r", true, false), "\r");
}

#[test]
fn native_shift_enter_leaves_non_return_input_unchanged() {
    assert_eq!(normalize_native_shift_enter_input("\x1b[13;2u", true, true), "\x1b[13;2u");
    assert_eq!(normalize_native_shift_enter_input("a", true, true), "a");
}

// normalizeAppleTerminalInput

#[test]
fn rewrites_apple_terminal_return_to_csi_u_shift_enter_when_shift_is_pressed() {
    assert_eq!(normalize_apple_terminal_input("\r", true, true), "\x1b[13;2u");
}

#[test]
fn leaves_apple_terminal_return_unchanged_when_shift_is_not_pressed() {
    assert_eq!(normalize_apple_terminal_input("\r", true, false), "\r");
}

#[test]
fn leaves_non_apple_terminal_return_unchanged_when_shift_is_pressed() {
    assert_eq!(normalize_apple_terminal_input("\r", false, true), "\r");
}

#[test]
fn apple_terminal_leaves_non_return_input_unchanged() {
    assert_eq!(normalize_apple_terminal_input("\x1b[13;2u", true, true), "\x1b[13;2u");
    assert_eq!(normalize_apple_terminal_input("a", true, true), "a");
}
