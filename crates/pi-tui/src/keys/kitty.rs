//! Kitty keyboard protocol (CSI u and CSI ~ forms) and xterm modifyOtherKeys parsing.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::constants::{
    CP_DELETE, CP_DOWN, CP_END, CP_F1, CP_HOME, CP_INSERT, CP_LEFT, CP_PAGE_DOWN, CP_PAGE_UP, CP_RIGHT, CP_UP,
    LOCK_MASK, SHIFTED_SYMBOL_KEYS, SYMBOL_KEYS, int32, is_symbol_code, logical_shifted_symbol_candidate,
    normalize_kitty_functional_codepoint, normalize_shifted_letter_identity_codepoint, tilde_function_key_codepoint,
};

/// Event type reported by the Kitty protocol with flag 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyEventType {
    /// Key press (event 1, or no event field).
    Press,
    /// Key auto-repeat (event 2).
    Repeat,
    /// Key release (event 3).
    Release,
}

/// CSI u: `codepoint[:shifted[:base]][;mod[:event]]u`, shared with printable decoding.
pub(crate) static KITTY_CSI_U: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\x1b\[([0-9]+)(?::([0-9]*))?(?::([0-9]+))?(?:;([0-9]+))?(?::([0-9]+))?u$").expect("valid regex")
});
static KITTY_ARROW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[1;([0-9]+)(?::([0-9]+))?([ABCD])$").expect("valid regex"));
static KITTY_FUNCTIONAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[([0-9]+)(?:;([0-9]+))?(?::([0-9]+))?~$").expect("valid regex"));
static KITTY_HOME_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[1;([0-9]+)(?::([0-9]+))?([HF])$").expect("valid regex"));
/// F1, F2, and F4 as `CSI P`, `CSI Q`, and `CSI S`, or `CSI 1 ; mod[:event] P` with modifiers.
static KITTY_FUNCTION_LETTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[(?:1;([0-9]+)(?::([0-9]+))?)?([PQS])$").expect("valid regex"));
static MODIFY_OTHER_KEYS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[27;([0-9]+);([0-9]+)~$").expect("valid regex"));

/// A decoded Kitty key event. Modifiers are zero-based bitmasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParsedKittySequence {
    pub codepoint: i64,
    pub shifted_key: Option<i64>,
    pub base_layout_key: Option<i64>,
    pub modifier: i64,
    pub event_type: KeyEventType,
}

/// A decoded xterm modifyOtherKeys event (`CSI 27 ; mod ; keycode ~`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParsedModifyOtherKeys {
    pub codepoint: i64,
    pub modifier: i64,
}

/// Parses an ASCII digit run. Values that do not fit in `i64` make the sequence unrecognized.
pub(crate) fn parse_number(digits: &str) -> Option<i64> {
    digits.parse::<i64>().ok()
}

fn group<'a>(captures: &Captures<'a>, index: usize) -> Option<&'a str> {
    captures.get(index).map(|m| m.as_str())
}

fn parse_event_type(event: Option<&str>) -> KeyEventType {
    match event.filter(|s| !s.is_empty()).and_then(parse_number) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

fn modifier_or_default(value: Option<&str>) -> Option<i64> {
    match value {
        Some(digits) => Some(parse_number(digits)? - 1),
        None => Some(0),
    }
}

/// Parses Kitty CSI u, arrows and Home/End with modifiers, and functional `~` keys.
pub(crate) fn parse_kitty_sequence(data: &str) -> Option<ParsedKittySequence> {
    if let Some(c) = KITTY_CSI_U.captures(data) {
        let codepoint = parse_number(&c[1])?;
        let shifted_key = match group(&c, 2).filter(|s| !s.is_empty()) {
            Some(digits) => Some(parse_number(digits)?),
            None => None,
        };
        let base_layout_key = match group(&c, 3) {
            Some(digits) => Some(parse_number(digits)?),
            None => None,
        };
        let modifier = modifier_or_default(group(&c, 4))?;
        let event_type = parse_event_type(group(&c, 5));
        return Some(ParsedKittySequence { codepoint, shifted_key, base_layout_key, modifier, event_type });
    }

    if let Some(c) = KITTY_ARROW.captures(data) {
        let modifier = parse_number(&c[1])? - 1;
        let event_type = parse_event_type(group(&c, 2));
        let codepoint = match &c[3] {
            "A" => CP_UP,
            "B" => CP_DOWN,
            "C" => CP_RIGHT,
            _ => CP_LEFT,
        };
        return Some(simple(codepoint, modifier, event_type));
    }

    if let Some(c) = KITTY_FUNCTIONAL.captures(data) {
        let key_number = parse_number(&c[1])?;
        let modifier = modifier_or_default(group(&c, 2))?;
        let event_type = parse_event_type(group(&c, 3));
        let codepoint = match key_number {
            2 => Some(CP_INSERT),
            3 => Some(CP_DELETE),
            5 => Some(CP_PAGE_UP),
            6 => Some(CP_PAGE_DOWN),
            7 => Some(CP_HOME),
            8 => Some(CP_END),
            _ => tilde_function_key_codepoint(key_number),
        };
        if let Some(codepoint) = codepoint {
            return Some(simple(codepoint, modifier, event_type));
        }
    }

    if let Some(c) = KITTY_HOME_END.captures(data) {
        let modifier = parse_number(&c[1])? - 1;
        let event_type = parse_event_type(group(&c, 2));
        let codepoint = if &c[3] == "H" { CP_HOME } else { CP_END };
        return Some(simple(codepoint, modifier, event_type));
    }

    if let Some(c) = KITTY_FUNCTION_LETTER.captures(data) {
        let modifier = modifier_or_default(group(&c, 1))?;
        let event_type = parse_event_type(group(&c, 2));
        let codepoint = match &c[3] {
            "P" => CP_F1,
            "Q" => CP_F1 - 1,
            _ => CP_F1 - 3,
        };
        return Some(simple(codepoint, modifier, event_type));
    }

    None
}

fn simple(codepoint: i64, modifier: i64, event_type: KeyEventType) -> ParsedKittySequence {
    ParsedKittySequence { codepoint, shifted_key: None, base_layout_key: None, modifier, event_type }
}

/// Parses `CSI 27 ; mod ; keycode ~`.
pub(crate) fn parse_modify_other_keys_sequence(data: &str) -> Option<ParsedModifyOtherKeys> {
    let c = MODIFY_OTHER_KEYS.captures(data)?;
    let modifier = parse_number(&c[1])? - 1;
    let codepoint = parse_number(&c[2])?;
    Some(ParsedModifyOtherKeys { codepoint, modifier })
}

/// Returns the Kitty event type of `data` when it is a Kitty key sequence.
pub fn kitty_event_type(data: &str) -> Option<KeyEventType> {
    parse_kitty_sequence(data).map(|parsed| parsed.event_type)
}

const PASTE_START: &str = "\x1b[200~";
const EVENT_FINALS: [char; 11] = ['u', '~', 'A', 'B', 'C', 'D', 'H', 'F', 'P', 'Q', 'S'];

fn has_event_marker(data: &str, event: char) -> bool {
    data.match_indices(':').any(|(index, _)| {
        let mut rest = data[index + 1..].chars();
        rest.next() == Some(event) && rest.next().is_some_and(|c| EVENT_FINALS.contains(&c))
    })
}

/// Returns true when `data` is a Kitty key release event (flag 2). Bracketed paste is never a release.
pub fn is_key_release(data: &str) -> bool {
    !data.contains(PASTE_START) && has_event_marker(data, '3')
}

/// Returns true when `data` is a Kitty key repeat event (flag 2). Bracketed paste is never a repeat.
pub fn is_key_repeat(data: &str) -> bool {
    !data.contains(PASTE_START) && has_event_marker(data, '2')
}

/// Matches a Kitty sequence against a codepoint and zero-based modifier.
pub(crate) fn matches_kitty_sequence(data: &str, expected_codepoint: i64, expected_modifier: i64) -> bool {
    let Some(parsed) = parse_kitty_sequence(data) else {
        return false;
    };
    let actual_mod = int32(parsed.modifier) & !LOCK_MASK;
    let expected_mod = int32(expected_modifier) & !LOCK_MASK;

    let normalized = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(parsed.codepoint),
        parsed.modifier,
    );
    let normalized_expected = normalize_shifted_letter_identity_codepoint(
        normalize_kitty_functional_codepoint(expected_codepoint),
        expected_modifier,
    );

    if actual_mod == expected_mod && normalized == normalized_expected {
        return true;
    }

    // Logical match: Kitty reports "+" as Shift plus the physical "=" key, so the binding
    // names the produced symbol and the implicit Shift is consumed. Ctrl, Alt, Super stay.
    if let Some((codepoint, modifier)) = logical_shifted_symbol_candidate(parsed.shifted_key, actual_mod, SYMBOL_KEYS) {
        let expected = normalize_kitty_functional_codepoint(expected_codepoint);
        if int32(modifier) & !LOCK_MASK == expected_mod && codepoint == expected {
            return true;
        }
    }

    if actual_mod != expected_mod {
        return false;
    }

    // The base layout key (non-Latin layouts) only counts when the codepoint is not already a
    // Latin letter or known symbol, so remapped layouts such as Dvorak do not match falsely.
    if parsed.base_layout_key == Some(expected_codepoint) {
        let is_latin_letter = (97..=122).contains(&normalized);
        if !is_latin_letter && !is_symbol_code(normalized, SYMBOL_KEYS) {
            return true;
        }
    }

    false
}

/// Matches modifyOtherKeys with an exact keycode and modifier.
pub(crate) fn matches_modify_other_keys(data: &str, expected_keycode: i64, expected_modifier: i64) -> bool {
    parse_modify_other_keys_sequence(data)
        .is_some_and(|parsed| parsed.codepoint == expected_keycode && parsed.modifier == expected_modifier)
}

/// Matches modifyOtherKeys for printable keys, accepting uppercase letters and shifted symbols.
pub(crate) fn matches_printable_modify_other_keys(data: &str, expected_keycode: i64, expected_modifier: i64) -> bool {
    let Some(parsed) = parse_modify_other_keys_sequence(data) else {
        return false;
    };
    let physical_match = parsed.modifier == expected_modifier
        && normalize_shifted_letter_identity_codepoint(parsed.codepoint, parsed.modifier)
            == normalize_shifted_letter_identity_codepoint(expected_keycode, expected_modifier);
    if physical_match {
        return true;
    }
    logical_shifted_symbol_candidate(Some(parsed.codepoint), parsed.modifier, SHIFTED_SYMBOL_KEYS)
        .is_some_and(|(codepoint, modifier)| modifier == expected_modifier && codepoint == expected_keycode)
}
