//! `parse_key` (input to key id) and printable decoding of Kitty and modifyOtherKeys input.

use super::constants::{
    ALT, CP_BACKSPACE, CP_DELETE, CP_DOWN, CP_END, CP_ENTER, CP_ESCAPE, CP_HOME, CP_INSERT, CP_KP_ENTER, CP_LEFT,
    CP_PAGE_DOWN, CP_PAGE_UP, CP_RIGHT, CP_SPACE, CP_TAB, CP_UP, CTRL, LOCK_MASK, SHIFT, SHIFTED_SYMBOL_KEYS, SUPER,
    SYMBOL_KEYS, char_code_unit, int32, is_symbol_code, legacy_sequence_key_id, logical_shifted_symbol_candidate,
    normalize_kitty_functional_codepoint, normalize_shifted_letter_identity_codepoint,
};
use super::is_kitty_protocol_active;
use super::kitty::{KITTY_CSI_U, parse_kitty_sequence, parse_modify_other_keys_sequence, parse_number};
use super::matching::is_windows_terminal_session;
use crate::env::{EnvLookup, ProcessEnv};

fn format_key_name_with_modifiers(key_name: &str, modifier: i64) -> Option<String> {
    let effective = int32(modifier) & !LOCK_MASK;
    if effective & !(SHIFT | CTRL | ALT | SUPER) != 0 {
        return None;
    }
    let mut out = String::new();
    for (bit, name) in [(SHIFT, "shift+"), (CTRL, "ctrl+"), (ALT, "alt+"), (SUPER, "super+")] {
        if effective & bit != 0 {
            out.push_str(name);
        }
    }
    out.push_str(key_name);
    Some(out)
}

/// The ASCII character for a code already known to be a letter, digit, or symbol.
fn ascii_name(code: i64) -> String {
    char::from(char_code_unit(code) as u8).to_string()
}

fn format_parsed_key(codepoint: i64, modifier: i64, base_layout_key: Option<i64>) -> Option<String> {
    let identity =
        normalize_shifted_letter_identity_codepoint(normalize_kitty_functional_codepoint(codepoint), modifier);

    // The base layout key only names keys whose codepoint is not a Latin letter, digit, or
    // known symbol; for those the codepoint wins so remapped layouts report the right name.
    let recognized =
        (97..=122).contains(&identity) || (48..=57).contains(&identity) || is_symbol_code(identity, SYMBOL_KEYS);
    let effective = if recognized { identity } else { base_layout_key.unwrap_or(identity) };

    let key_name = match effective {
        CP_ESCAPE => "escape".to_string(),
        CP_TAB => "tab".to_string(),
        CP_ENTER | CP_KP_ENTER => "enter".to_string(),
        CP_SPACE => "space".to_string(),
        CP_BACKSPACE => "backspace".to_string(),
        CP_DELETE => "delete".to_string(),
        CP_INSERT => "insert".to_string(),
        CP_HOME => "home".to_string(),
        CP_END => "end".to_string(),
        CP_PAGE_UP => "pageUp".to_string(),
        CP_PAGE_DOWN => "pageDown".to_string(),
        CP_UP => "up".to_string(),
        CP_DOWN => "down".to_string(),
        CP_LEFT => "left".to_string(),
        CP_RIGHT => "right".to_string(),
        48..=57 | 97..=122 => ascii_name(effective),
        _ if is_symbol_code(effective, SYMBOL_KEYS) => ascii_name(effective),
        _ => return None,
    };
    format_key_name_with_modifiers(&key_name, modifier)
}

/// Returns the key id for one input sequence (for example `"ctrl+c"`), or `None` when unknown.
/// Reads the process environment.
pub fn parse_key(data: &str) -> Option<String> {
    parse_key_with_env(data, &ProcessEnv)
}

/// [`parse_key`] with an explicit environment for the Windows Terminal backspace rule.
pub fn parse_key_with_env(data: &str, env: &dyn EnvLookup) -> Option<String> {
    if let Some(kitty) = parse_kitty_sequence(data) {
        let logical = logical_shifted_symbol_candidate(kitty.shifted_key, kitty.modifier, SYMBOL_KEYS);
        let (codepoint, modifier) = logical.unwrap_or((kitty.codepoint, kitty.modifier));
        return format_parsed_key(codepoint, modifier, kitty.base_layout_key);
    }

    if let Some(mok) = parse_modify_other_keys_sequence(data) {
        let logical = logical_shifted_symbol_candidate(Some(mok.codepoint), mok.modifier, SHIFTED_SYMBOL_KEYS);
        let (codepoint, modifier) = logical.unwrap_or((mok.codepoint, mok.modifier));
        return format_parsed_key(codepoint, modifier, None);
    }

    let kitty_active = is_kitty_protocol_active();
    // With Kitty active, ESC CR (Kitty mapping) and LF (Ghostty mapping) mean shift+enter.
    if kitty_active && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".to_string());
    }

    if let Some(key_id) = legacy_sequence_key_id(data) {
        return Some(key_id.to_string());
    }

    parse_legacy(data, kitty_active, env).map(str::to_string).or_else(|| parse_short(data, kitty_active))
}

fn parse_legacy(data: &str, kitty_active: bool, env: &dyn EnvLookup) -> Option<&'static str> {
    Some(match data {
        "\x1b" => "escape",
        "\x1c" => "ctrl+\\",
        "\x1d" => "ctrl+]",
        "\x1f" => "ctrl+-",
        "\x1b\x1b" => "ctrl+alt+[",
        "\x1b\x1c" => "ctrl+alt+\\",
        "\x1b\x1d" => "ctrl+alt+]",
        "\x1b\x1f" => "ctrl+alt+-",
        "\t" => "tab",
        "\r" | "\x1bOM" => "enter",
        "\n" if !kitty_active => "enter",
        "\x00" => "ctrl+space",
        " " => "space",
        "\x7f" => "backspace",
        "\x08" if is_windows_terminal_session(env) => "ctrl+backspace",
        "\x08" => "backspace",
        "\x1b[Z" => "shift+tab",
        "\x1b\r" if !kitty_active => "alt+enter",
        "\x1b " if !kitty_active => "alt+space",
        "\x1b\x7f" | "\x1b\x08" => "alt+backspace",
        "\x1bB" if !kitty_active => "alt+left",
        "\x1bF" if !kitty_active => "alt+right",
        _ => return None,
    })
}

/// Two-character `ESC x` forms (legacy Alt), CSI navigation, and single bytes.
fn parse_short(data: &str, kitty_active: bool) -> Option<String> {
    let mut chars = data.chars();
    let first = chars.next()?;
    let second = chars.next();
    let single = second.is_none();
    let pair = second.is_some() && chars.next().is_none();

    if !kitty_active && pair && first == '\x1b' {
        let code = second.map_or(0, u32::from);
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", char::from(code as u8 + 96)));
        }
        // Legacy alt+letter/digit/symbol is ESC followed by the key.
        let key = second.map(String::from).unwrap_or_default();
        if (97..=122).contains(&code) || (48..=57).contains(&code) || is_symbol_code(i64::from(code), SYMBOL_KEYS) {
            return Some(format!("alt+{key}"));
        }
    }

    let navigation = match data {
        "\x1b[A" => Some("up"),
        "\x1b[B" => Some("down"),
        "\x1b[C" => Some("right"),
        "\x1b[D" => Some("left"),
        "\x1b[H" | "\x1bOH" => Some("home"),
        "\x1b[F" | "\x1bOF" => Some("end"),
        "\x1b[3~" => Some("delete"),
        "\x1b[5~" => Some("pageUp"),
        "\x1b[6~" => Some("pageDown"),
        _ => None,
    };
    if let Some(name) = navigation {
        return Some(name.to_string());
    }

    if single {
        let code = u32::from(first);
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", char::from(code as u8 + 96)));
        }
        if (32..=126).contains(&code) {
            return Some(data.to_string());
        }
    }
    None
}

/// Kitty modifier bits allowed for printable input: Shift and the lock keys.
const KITTY_PRINTABLE_ALLOWED_MODIFIERS: i64 = SHIFT | LOCK_MASK;

fn char_from_codepoint(codepoint: i64) -> Option<String> {
    u32::try_from(codepoint).ok().and_then(char::from_u32).map(String::from)
}

/// Decodes a Kitty CSI u sequence for a plain key or a key with Shift into the character it
/// types. Rejects Ctrl, Alt, Super, and control characters; prefers the shifted key with Shift.
pub fn decode_kitty_printable(data: &str) -> Option<String> {
    let c = KITTY_CSI_U.captures(data)?;
    let codepoint = parse_number(&c[1])?;
    let shifted_key = match c.get(2).map(|m| m.as_str()).filter(|s| !s.is_empty()) {
        Some(digits) => Some(parse_number(digits)?),
        None => None,
    };
    let modifier = match c.get(4) {
        Some(m) => parse_number(m.as_str())? - 1,
        None => 0,
    };
    let bits = int32(modifier);
    if bits & !KITTY_PRINTABLE_ALLOWED_MODIFIERS != 0 || bits & (ALT | CTRL) != 0 {
        return None;
    }
    let mut effective = codepoint;
    if bits & SHIFT != 0 {
        if let Some(shifted) = shifted_key {
            effective = shifted;
        }
    }
    let effective = normalize_kitty_functional_codepoint(effective);
    if effective < 32 {
        return None;
    }
    char_from_codepoint(effective)
}

fn decode_modify_other_keys_printable(data: &str) -> Option<String> {
    let parsed = parse_modify_other_keys_sequence(data)?;
    let modifier = int32(parsed.modifier) & !LOCK_MASK;
    if modifier & !SHIFT != 0 || parsed.codepoint < 32 {
        return None;
    }
    char_from_codepoint(parsed.codepoint)
}

/// Decodes printable text from a Kitty CSI u or xterm modifyOtherKeys sequence.
pub fn decode_printable_key(data: &str) -> Option<String> {
    decode_kitty_printable(data).or_else(|| decode_modify_other_keys_printable(data))
}
