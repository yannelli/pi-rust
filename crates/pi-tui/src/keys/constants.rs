//! Codepoints, modifier bits, and legacy escape sequence tables shared by the key parsers.

/// Symbol keys that can appear in a key id.
pub(crate) const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

/// Symbols that a terminal can report as Shift plus another physical key.
pub(crate) const SHIFTED_SYMBOL_KEYS: &str = "~!@#$%^&*()_+|{}:<>?";

pub(crate) const SHIFT: i64 = 1;
pub(crate) const ALT: i64 = 2;
pub(crate) const CTRL: i64 = 4;
pub(crate) const SUPER: i64 = 8;

/// Caps Lock and Num Lock bits.
pub(crate) const LOCK_MASK: i64 = 64 + 128;

/// Modifier names in the order key ids are scanned, with their bits.
pub(crate) const MODIFIER_NAMES: [(&str, i64); 4] = [("shift", SHIFT), ("alt", ALT), ("ctrl", CTRL), ("super", SUPER)];

pub(crate) const CP_ESCAPE: i64 = 27;
pub(crate) const CP_TAB: i64 = 9;
pub(crate) const CP_ENTER: i64 = 13;
pub(crate) const CP_SPACE: i64 = 32;
pub(crate) const CP_BACKSPACE: i64 = 127;
/// Numpad Enter in the Kitty protocol.
pub(crate) const CP_KP_ENTER: i64 = 57414;

pub(crate) const CP_UP: i64 = -1;
pub(crate) const CP_DOWN: i64 = -2;
pub(crate) const CP_RIGHT: i64 = -3;
pub(crate) const CP_LEFT: i64 = -4;

pub(crate) const CP_DELETE: i64 = -10;
pub(crate) const CP_INSERT: i64 = -11;
pub(crate) const CP_PAGE_UP: i64 = -12;
pub(crate) const CP_PAGE_DOWN: i64 = -13;
pub(crate) const CP_HOME: i64 = -14;
pub(crate) const CP_END: i64 = -15;

/// F1 to F12 take internal codepoints -21 (F1) down to -32 (F12), as upstream.
pub(crate) const CP_F1: i64 = -21;
pub(crate) const CP_F12: i64 = CP_F1 - 11;

/// Kitty reserves this Private Use Area range for functional keys (F13 to F35, media keys,
/// modifier keys); a CSI u codepoint in it is never text.
pub(crate) const KITTY_FUNCTIONAL_PUA: std::ops::RangeInclusive<i64> = 57344..=63743;

/// Codepoint of a function key name, `f1` to `f12`.
pub(crate) fn function_key_codepoint(key: &str) -> Option<i64> {
    let number: i64 = key.strip_prefix('f')?.parse().ok()?;
    (1..=12).contains(&number).then(|| CP_F1 - (number - 1))
}

/// Function key of a `CSI number ~` sequence. Kitty sends F3 as `CSI 13 ~`, since `CSI R`
/// is also a cursor position report.
pub(crate) fn tilde_function_key_codepoint(number: i64) -> Option<i64> {
    let index = match number {
        11..=15 => number - 11,
        17..=21 => number - 12,
        23 | 24 => number - 13,
        _ => return None,
    };
    Some(CP_F1 - index)
}

/// Applies JavaScript `ToInt32` so bitwise operations wrap the same way as upstream.
pub(crate) fn int32(value: i64) -> i64 {
    i64::from(value as i32)
}

/// Applies JavaScript `String.fromCharCode` truncation to a UTF-16 code unit.
pub(crate) fn char_code_unit(code: i64) -> u16 {
    code.rem_euclid(65536) as u16
}

/// Returns true when `String.fromCharCode(code)` is one of the symbols in `set`.
pub(crate) fn is_symbol_code(code: i64, set: &str) -> bool {
    let unit = char_code_unit(code);
    unit < 128 && set.as_bytes().contains(&(unit as u8))
}

/// Returns true when `key` is exactly one symbol key.
pub(crate) fn is_symbol_key(key: &str) -> bool {
    key.len() == 1 && SYMBOL_KEYS.contains(key)
}

/// Maps Kitty keypad functional codepoints to the logical key they produce.
pub(crate) fn normalize_kitty_functional_codepoint(codepoint: i64) -> i64 {
    match codepoint {
        57399..=57408 => codepoint - 57399 + 48,
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57415 => 61,
        57416 => 44,
        57417 => CP_LEFT,
        57418 => CP_RIGHT,
        57419 => CP_UP,
        57420 => CP_DOWN,
        57421 => CP_PAGE_UP,
        57422 => CP_PAGE_DOWN,
        57423 => CP_HOME,
        57424 => CP_END,
        57425 => CP_INSERT,
        57426 => CP_DELETE,
        _ => codepoint,
    }
}

/// Lowercases an uppercase ASCII letter codepoint when Shift is held.
pub(crate) fn normalize_shifted_letter_identity_codepoint(codepoint: i64, modifier: i64) -> i64 {
    let effective = int32(modifier) & !LOCK_MASK;
    if effective & SHIFT != 0 && (65..=90).contains(&codepoint) { codepoint + 32 } else { codepoint }
}

/// Resolves a shifted symbol to the produced symbol and the modifier without Shift.
pub(crate) fn logical_shifted_symbol_candidate(
    codepoint: Option<i64>,
    modifier: i64,
    allowed: &str,
) -> Option<(i64, i64)> {
    let codepoint = codepoint?;
    if int32(modifier) & SHIFT == 0 {
        return None;
    }
    let normalized = normalize_kitty_functional_codepoint(codepoint);
    if !is_symbol_code(normalized, allowed) {
        return None;
    }
    Some((normalized, int32(modifier) & !SHIFT))
}

/// Unmodified legacy sequences for a lowercase key name.
pub(crate) fn legacy_key_sequences(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageup" => &["\x1b[5~", "\x1b[[5~"],
        "pagedown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => &[],
    }
}

/// rxvt-style Shift sequences for a lowercase key name.
fn legacy_shift_sequences(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[a"],
        "down" => &["\x1b[b"],
        "right" => &["\x1b[c"],
        "left" => &["\x1b[d"],
        "clear" => &["\x1b[e"],
        "insert" => &["\x1b[2$"],
        "delete" => &["\x1b[3$"],
        "pageup" => &["\x1b[5$"],
        "pagedown" => &["\x1b[6$"],
        "home" => &["\x1b[7$"],
        "end" => &["\x1b[8$"],
        _ => &[],
    }
}

/// rxvt-style Ctrl sequences for a lowercase key name.
fn legacy_ctrl_sequences(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1bOa"],
        "down" => &["\x1bOb"],
        "right" => &["\x1bOc"],
        "left" => &["\x1bOd"],
        "clear" => &["\x1bOe"],
        "insert" => &["\x1b[2^"],
        "delete" => &["\x1b[3^"],
        "pageup" => &["\x1b[5^"],
        "pagedown" => &["\x1b[6^"],
        "home" => &["\x1b[7^"],
        "end" => &["\x1b[8^"],
        _ => &[],
    }
}

/// Returns true when `data` is one of the unmodified legacy sequences for `key`.
pub(crate) fn matches_legacy_sequence(data: &str, key: &str) -> bool {
    legacy_key_sequences(key).contains(&data)
}

/// Matches the rxvt Shift or Ctrl sequences; other modifiers never match.
pub(crate) fn matches_legacy_modifier_sequence(data: &str, key: &str, modifier: i64) -> bool {
    if modifier == SHIFT {
        return legacy_shift_sequences(key).contains(&data);
    }
    if modifier == CTRL {
        return legacy_ctrl_sequences(key).contains(&data);
    }
    false
}

/// Key ids for legacy sequences that `parse_key` resolves by table lookup.
pub(crate) fn legacy_sequence_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" | "\x1b[11~" | "\x1b[[A" => "f1",
        "\x1bOQ" | "\x1b[12~" | "\x1b[[B" => "f2",
        "\x1bOR" | "\x1b[13~" | "\x1b[[C" => "f3",
        "\x1bOS" | "\x1b[14~" | "\x1b[[D" => "f4",
        "\x1b[[E" | "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}
