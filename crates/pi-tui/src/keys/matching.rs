//! `matches_key`: tests raw terminal input against a key id across Kitty, modifyOtherKeys,
//! and legacy encodings.

use super::constants::{
    ALT, CP_BACKSPACE, CP_DELETE, CP_DOWN, CP_END, CP_ENTER, CP_ESCAPE, CP_HOME, CP_INSERT, CP_KP_ENTER, CP_LEFT,
    CP_PAGE_DOWN, CP_PAGE_UP, CP_RIGHT, CP_SPACE, CP_TAB, CP_UP, CTRL, SHIFT, function_key_codepoint, is_symbol_key,
    matches_legacy_modifier_sequence, matches_legacy_sequence,
};
use super::ids::parse_key_id;
use super::is_kitty_protocol_active;
use super::kitty::{matches_kitty_sequence, matches_modify_other_keys, matches_printable_modify_other_keys};
use crate::env::{EnvLookup, ProcessEnv};

/// Returns true in a local Windows Terminal session (not over SSH).
pub fn is_windows_terminal_session(env: &dyn EnvLookup) -> bool {
    env.non_empty("WT_SESSION").is_some()
        && env.non_empty("SSH_CONNECTION").is_none()
        && env.non_empty("SSH_CLIENT").is_none()
        && env.non_empty("SSH_TTY").is_none()
}

/// Raw 0x08 is Ctrl+Backspace in Windows Terminal but plain Backspace elsewhere; 0x7f is
/// always plain Backspace.
fn matches_raw_backspace(data: &str, expected_modifier: i64, env: &dyn EnvLookup) -> bool {
    match data {
        "\x7f" => expected_modifier == 0,
        "\x08" if is_windows_terminal_session(env) => expected_modifier == CTRL,
        "\x08" => expected_modifier == 0,
        _ => false,
    }
}

/// Control character for a key (`code & 0x1f`); `-` maps to the same byte as `_`.
fn raw_ctrl_char(key: &str) -> Option<char> {
    let c = key.chars().next()?.to_lowercase().next()?;
    match c {
        'a'..='z' | '[' | '\\' | ']' | '_' => Some(char::from(c as u8 & 0x1f)),
        '-' => Some('\x1f'),
        _ => None,
    }
}

fn kitty_or_mok(data: &str, codepoint: i64, modifier: i64) -> bool {
    matches_kitty_sequence(data, codepoint, modifier) || matches_modify_other_keys(data, codepoint, modifier)
}

/// Shared rule for navigation keys: legacy forms first, then Kitty.
fn matches_navigation(data: &str, name: &str, codepoint: i64, modifier: i64) -> bool {
    if modifier == 0 {
        return matches_legacy_sequence(data, name) || matches_kitty_sequence(data, codepoint, 0);
    }
    matches_legacy_modifier_sequence(data, name, modifier) || matches_kitty_sequence(data, codepoint, modifier)
}

/// Function keys: the legacy forms without modifiers, then the Kitty and xterm forms, such as
/// `CSI P` for F1, `CSI 1;5P` for Ctrl+F1, and `CSI 15;5~` for Ctrl+F5.
fn matches_function_key(data: &str, key: &str, modifier: i64) -> bool {
    (modifier == 0 && matches_legacy_sequence(data, key))
        || function_key_codepoint(key).is_some_and(|codepoint| matches_kitty_sequence(data, codepoint, modifier))
}

fn matches_space(data: &str, modifier: i64, kitty: bool) -> bool {
    if !kitty && ((modifier == CTRL && data == "\x00") || (modifier == ALT && data == "\x1b ")) {
        return true;
    }
    if modifier == 0 {
        return data == " " || kitty_or_mok(data, CP_SPACE, 0);
    }
    kitty_or_mok(data, CP_SPACE, modifier)
}

fn matches_tab(data: &str, modifier: i64) -> bool {
    if modifier == SHIFT {
        return data == "\x1b[Z" || kitty_or_mok(data, CP_TAB, SHIFT);
    }
    if modifier == 0 {
        return data == "\t" || matches_kitty_sequence(data, CP_TAB, 0);
    }
    kitty_or_mok(data, CP_TAB, modifier)
}

fn matches_enter_csi(data: &str, modifier: i64) -> bool {
    matches_kitty_sequence(data, CP_ENTER, modifier) || matches_kitty_sequence(data, CP_KP_ENTER, modifier)
}

fn matches_enter(data: &str, modifier: i64, kitty: bool) -> bool {
    if modifier == SHIFT {
        if matches_enter_csi(data, SHIFT) || matches_modify_other_keys(data, CP_ENTER, SHIFT) {
            return true;
        }
        // With Kitty active, ESC CR is Kitty's shift+enter mapping and LF is Ghostty's.
        return kitty && (data == "\x1b\r" || data == "\n");
    }
    if modifier == ALT {
        if matches_enter_csi(data, ALT) || matches_modify_other_keys(data, CP_ENTER, ALT) {
            return true;
        }
        // ESC CR is alt+enter only in legacy mode; Kitty sends alt+enter as CSI u.
        return !kitty && data == "\x1b\r";
    }
    if modifier == 0 {
        return data == "\r" || (!kitty && data == "\n") || data == "\x1bOM" || matches_enter_csi(data, 0);
    }
    matches_enter_csi(data, modifier) || matches_modify_other_keys(data, CP_ENTER, modifier)
}

fn matches_backspace(data: &str, modifier: i64, env: &dyn EnvLookup) -> bool {
    if modifier == ALT {
        return data == "\x1b\x7f" || data == "\x1b\x08" || kitty_or_mok(data, CP_BACKSPACE, ALT);
    }
    if modifier == CTRL {
        // Raw 0x08 is ambiguous: Ctrl+Backspace in Windows Terminal, plain Backspace elsewhere,
        // and also Ctrl+H.
        return matches_raw_backspace(data, CTRL, env) || kitty_or_mok(data, CP_BACKSPACE, CTRL);
    }
    if modifier == 0 {
        return matches_raw_backspace(data, 0, env) || kitty_or_mok(data, CP_BACKSPACE, 0);
    }
    kitty_or_mok(data, CP_BACKSPACE, modifier)
}

/// Horizontal arrows accept extra legacy Alt and Ctrl forms (`ESC b`, `ESC B`, `CSI 1;5D`).
fn matches_horizontal_arrow(data: &str, modifier: i64, kitty: bool, left: bool) -> bool {
    let (name, codepoint) = if left { ("left", CP_LEFT) } else { ("right", CP_RIGHT) };
    if modifier == ALT {
        let (csi, shifted_meta, meta) =
            if left { ("\x1b[1;3D", "\x1bB", "\x1bb") } else { ("\x1b[1;3C", "\x1bF", "\x1bf") };
        return data == csi
            || (!kitty && data == shifted_meta)
            || data == meta
            || matches_kitty_sequence(data, codepoint, ALT);
    }
    if modifier == CTRL {
        let csi = if left { "\x1b[1;5D" } else { "\x1b[1;5C" };
        return data == csi
            || matches_legacy_modifier_sequence(data, name, CTRL)
            || matches_kitty_sequence(data, codepoint, CTRL);
    }
    matches_navigation(data, name, codepoint, modifier)
}

fn matches_printable(data: &str, key: &str, modifier: i64, kitty: bool) -> bool {
    let byte = match key.as_bytes() {
        [byte] => *byte,
        _ => return false,
    };
    let is_letter = byte.is_ascii_lowercase();
    let is_digit = byte.is_ascii_digit();
    if !is_letter && !is_digit && !is_symbol_key(key) {
        return false;
    }
    let codepoint = i64::from(byte);
    let raw_ctrl = raw_ctrl_char(key);

    if modifier == CTRL + ALT && !kitty {
        // Legacy ctrl+alt+key is ESC plus the control byte. Otherwise fall through so CSI u
        // and modifyOtherKeys forms from tmux still match.
        if let Some(ctrl) = raw_ctrl {
            let mut chars = data.chars();
            if chars.next() == Some('\x1b') && chars.next() == Some(ctrl) && chars.next().is_none() {
                return true;
            }
        }
    }

    if modifier == ALT && !kitty && data.strip_prefix('\x1b') == Some(key) {
        return true;
    }

    let kitty_or_printable_mok = |modifier: i64| {
        matches_kitty_sequence(data, codepoint, modifier)
            || matches_printable_modify_other_keys(data, codepoint, modifier)
    };

    if modifier == CTRL {
        if raw_ctrl.is_some_and(|ctrl| data.len() == 1 && data.starts_with(ctrl)) {
            return true;
        }
        return kitty_or_printable_mok(CTRL);
    }
    if modifier == SHIFT + CTRL {
        return kitty_or_printable_mok(SHIFT + CTRL);
    }
    if modifier == SHIFT {
        if is_letter && data.len() == 1 && data.as_bytes()[0] == byte.to_ascii_uppercase() {
            return true;
        }
        return kitty_or_printable_mok(SHIFT);
    }
    if modifier != 0 {
        return kitty_or_printable_mok(modifier);
    }
    data == key || kitty_or_printable_mok(0)
}

/// Returns true when `data` (one input sequence) is the key named by `key_id`, such as
/// `"ctrl+c"`, `"shift+enter"`, or `"ctrl+shift+p"`. Reads the process environment.
pub fn matches_key(data: &str, key_id: &str) -> bool {
    matches_key_with_env(data, key_id, &ProcessEnv)
}

/// [`matches_key`] with an explicit environment for the Windows Terminal backspace rule.
pub fn matches_key_with_env(data: &str, key_id: &str, env: &dyn EnvLookup) -> bool {
    let Some(parsed) = parse_key_id(key_id) else {
        return false;
    };
    let modifier = parsed.modifier;
    let kitty = is_kitty_protocol_active();
    let key = parsed.key.as_ref();

    match key {
        "escape" | "esc" => modifier == 0 && (data == "\x1b" || kitty_or_mok(data, CP_ESCAPE, 0)),
        "space" => matches_space(data, modifier, kitty),
        "tab" => matches_tab(data, modifier),
        "enter" | "return" => matches_enter(data, modifier, kitty),
        "backspace" => matches_backspace(data, modifier, env),
        "insert" => matches_navigation(data, key, CP_INSERT, modifier),
        "delete" => matches_navigation(data, key, CP_DELETE, modifier),
        "home" => matches_navigation(data, key, CP_HOME, modifier),
        "end" => matches_navigation(data, key, CP_END, modifier),
        "pageup" => matches_navigation(data, key, CP_PAGE_UP, modifier),
        "pagedown" => matches_navigation(data, key, CP_PAGE_DOWN, modifier),
        "clear" if modifier == 0 => matches_legacy_sequence(data, key),
        "clear" => matches_legacy_modifier_sequence(data, key, modifier),
        "up" if modifier == ALT => data == "\x1bp" || matches_kitty_sequence(data, CP_UP, ALT),
        "up" => matches_navigation(data, key, CP_UP, modifier),
        "down" if modifier == ALT => data == "\x1bn" || matches_kitty_sequence(data, CP_DOWN, ALT),
        "down" => matches_navigation(data, key, CP_DOWN, modifier),
        "left" => matches_horizontal_arrow(data, modifier, kitty, true),
        "right" => matches_horizontal_arrow(data, modifier, kitty, false),
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            matches_function_key(data, key, modifier)
        }
        _ => matches_printable(data, key, modifier, kitty),
    }
}
