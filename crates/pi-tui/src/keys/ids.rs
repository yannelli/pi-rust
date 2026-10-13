//! Key identifiers such as `ctrl+shift+p`, the [`Key`] helper, and key id parsing.

use std::borrow::Cow;

use super::constants::MODIFIER_NAMES;

/// An owned key identifier such as `"ctrl+c"` or `"pageUp"`.
pub type KeyId = String;

/// Helper for building key identifiers, mirroring upstream `Key`: constants name base keys
/// (`Key::ESCAPE`), and functions add modifiers (`Key::ctrl("c")`, `Key::ctrl_shift("p")`).
#[derive(Debug, Clone, Copy)]
pub struct Key;

macro_rules! key_consts {
    ($($(#[$doc:meta])* $name:ident = $value:literal;)*) => {
        impl Key {
            $($(#[$doc])* pub const $name: &'static str = $value;)*
            /// Every named key constant, in declaration order.
            pub const NAMED: &'static [&'static str] = &[$($value),*];
        }
    };
}

key_consts! {
    /// The Escape key.
    ESCAPE = "escape";
    /// Alias of Escape.
    ESC = "esc";
    /// The Enter key.
    ENTER = "enter";
    /// Alias of Enter.
    RETURN = "return";
    /// The Tab key.
    TAB = "tab";
    /// The space bar.
    SPACE = "space";
    /// The Backspace key.
    BACKSPACE = "backspace";
    /// The Delete key.
    DELETE = "delete";
    /// The Insert key.
    INSERT = "insert";
    /// The Clear key (keypad 5 without Num Lock).
    CLEAR = "clear";
    /// The Home key.
    HOME = "home";
    /// The End key.
    END = "end";
    /// The Page Up key.
    PAGE_UP = "pageUp";
    /// The Page Down key.
    PAGE_DOWN = "pageDown";
    /// The Up arrow.
    UP = "up";
    /// The Down arrow.
    DOWN = "down";
    /// The Left arrow.
    LEFT = "left";
    /// The Right arrow.
    RIGHT = "right";
    /// Function key F1.
    F1 = "f1";
    /// Function key F2.
    F2 = "f2";
    /// Function key F3.
    F3 = "f3";
    /// Function key F4.
    F4 = "f4";
    /// Function key F5.
    F5 = "f5";
    /// Function key F6.
    F6 = "f6";
    /// Function key F7.
    F7 = "f7";
    /// Function key F8.
    F8 = "f8";
    /// Function key F9.
    F9 = "f9";
    /// Function key F10.
    F10 = "f10";
    /// Function key F11.
    F11 = "f11";
    /// Function key F12.
    F12 = "f12";
    /// The `` ` `` key.
    BACKTICK = "`";
    /// The `-` key.
    HYPHEN = "-";
    /// The `=` key.
    EQUALS = "=";
    /// The `[` key.
    LEFTBRACKET = "[";
    /// The `]` key.
    RIGHTBRACKET = "]";
    /// The `\` key.
    BACKSLASH = "\\";
    /// The `;` key.
    SEMICOLON = ";";
    /// The `'` key.
    QUOTE = "'";
    /// The `,` key.
    COMMA = ",";
    /// The `.` key.
    PERIOD = ".";
    /// The `/` key.
    SLASH = "/";
    /// The `!` symbol.
    EXCLAMATION = "!";
    /// The `@` symbol.
    AT = "@";
    /// The `#` symbol.
    HASH = "#";
    /// The `$` symbol.
    DOLLAR = "$";
    /// The `%` symbol.
    PERCENT = "%";
    /// The `^` symbol.
    CARET = "^";
    /// The `&` symbol.
    AMPERSAND = "&";
    /// The `*` symbol.
    ASTERISK = "*";
    /// The `(` symbol.
    LEFTPAREN = "(";
    /// The `)` symbol.
    RIGHTPAREN = ")";
    /// The `_` symbol.
    UNDERSCORE = "_";
    /// The `+` symbol.
    PLUS = "+";
    /// The `|` symbol.
    PIPE = "|";
    /// The `~` symbol.
    TILDE = "~";
    /// The `{` symbol.
    LEFTBRACE = "{";
    /// The `}` symbol.
    RIGHTBRACE = "}";
    /// The `:` symbol.
    COLON = ":";
    /// The `<` symbol.
    LESSTHAN = "<";
    /// The `>` symbol.
    GREATERTHAN = ">";
    /// The `?` symbol.
    QUESTION = "?";
}

macro_rules! key_modifier_fns {
    ($($(#[$doc:meta])* $name:ident => $prefix:literal;)*) => {
        impl Key {
            $($(#[$doc])* pub fn $name(key: &str) -> KeyId { format!(concat!($prefix, "+{}"), key) })*
        }
    };
}

key_modifier_fns! {
    /// `ctrl+<key>`.
    ctrl => "ctrl";
    /// `shift+<key>`.
    shift => "shift";
    /// `alt+<key>`.
    alt => "alt";
    /// `super+<key>` (upstream `Key.super`; `super` is a Rust keyword).
    super_ => "super";
    /// `ctrl+shift+<key>`.
    ctrl_shift => "ctrl+shift";
    /// `shift+ctrl+<key>`.
    shift_ctrl => "shift+ctrl";
    /// `ctrl+alt+<key>`.
    ctrl_alt => "ctrl+alt";
    /// `alt+ctrl+<key>`.
    alt_ctrl => "alt+ctrl";
    /// `shift+alt+<key>`.
    shift_alt => "shift+alt";
    /// `alt+shift+<key>`.
    alt_shift => "alt+shift";
    /// `ctrl+super+<key>`.
    ctrl_super => "ctrl+super";
    /// `super+ctrl+<key>`.
    super_ctrl => "super+ctrl";
    /// `shift+super+<key>`.
    shift_super => "shift+super";
    /// `super+shift+<key>`.
    super_shift => "super+shift";
    /// `alt+super+<key>`.
    alt_super => "alt+super";
    /// `super+alt+<key>`.
    super_alt => "super+alt";
    /// `ctrl+shift+alt+<key>`.
    ctrl_shift_alt => "ctrl+shift+alt";
    /// `ctrl+shift+super+<key>`.
    ctrl_shift_super => "ctrl+shift+super";
}

/// A key id split into its lowercase base key and zero-based modifier bitmask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedKeyId<'a> {
    pub key: Cow<'a, str>,
    pub modifier: i64,
}

/// Parses `mod+mod+key`. Returns `None` for repeated modifiers, an empty key, or a stray `+`.
pub(crate) fn parse_key_id(key_id: &str) -> Option<ParsedKeyId<'_>> {
    let lowered: Cow<'_, str> = if key_id.is_ascii() && !key_id.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Borrowed(key_id)
    } else {
        Cow::Owned(key_id.to_lowercase())
    };
    let mut start = 0;
    let mut modifier = 0;
    loop {
        let rest = &lowered[start..];
        let found =
            MODIFIER_NAMES.iter().find(|(name, _)| rest.starts_with(name) && rest[name.len()..].starts_with('+'));
        let Some(&(name, bit)) = found else {
            break;
        };
        if modifier & bit != 0 {
            return None;
        }
        modifier |= bit;
        start += name.len() + 1;
    }
    let key = &lowered[start..];
    if key.is_empty() || (key.contains('+') && key != "+") {
        return None;
    }
    let key = match lowered {
        Cow::Borrowed(text) => Cow::Borrowed(&text[start..]),
        Cow::Owned(text) => Cow::Owned(text[start..].to_string()),
    };
    Some(ParsedKeyId { key, modifier })
}
