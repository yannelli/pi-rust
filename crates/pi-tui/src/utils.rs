//! Terminal text helpers: display width, ANSI-aware wrapping, truncation, and column slicing.
//!
//! Positions in strings are UTF-8 byte offsets; widths and columns are terminal cells.

mod ansi;
mod char_class_table;
mod eaw_table;
mod emoji;
mod emoji_table;
mod osc8;
mod segment;
mod slice;
mod tracker;
mod truncate;
mod width;
mod wrap;

use std::sync::LazyLock;

use regex::Regex;

pub use ansi::{extract_ansi_code, normalize_terminal_output, replace_control_characters, strip_terminal_sequences};
pub use emoji::is_rgi_emoji;
pub use osc8::get_osc8_link_at_column;
pub use segment::{WordSegment, graphemes, word_segments};
pub use slice::{
    GraphemeCellRange, LineSegments, SlicedText, extract_segments, get_grapheme_cell_range, slice_by_column,
    slice_with_width,
};
pub use tracker::{AnsiCodeTracker, get_active_background_ansi};
pub use truncate::{truncate_to_width, truncate_to_width_with};
pub use width::{east_asian_width, grapheme_width, visible_width};
pub use wrap::{apply_background_to_line, flatten_lines, wrap_text_with_ansi};

/// Character class body for the scripts whose text may break between any two graphemes.
const CJK_CLASS: &str = r"\p{scx=Han}\p{scx=Hiragana}\p{scx=Katakana}\p{scx=Hangul}\p{scx=Bopomofo}";

/// Full-width and typographic punctuation that separates CJK prose from completions.
const CJK_EXTRA_PUNCTUATION: &str = r"\x{FF0C}\x{FF0E}\x{FF1A}\x{FF1B}\x{FF01}\x{FF1F}\x{FF08}\x{FF09}\x{FF3B}\x{FF3D}\x{FF5B}\x{FF5D}\x{201C}\x{201D}\x{2018}\x{2019}\x{2026}\x{2014}";

/// The characters JavaScript's `\s` matches.
const JS_WHITESPACE_CLASS: &str =
    r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid built-in regex {pattern:?}: {err}"))
}

fn cjk_punctuation_source() -> String {
    format!(r"(?:[\p{{Punctuation}}&&[{CJK_CLASS}]]|[{CJK_EXTRA_PUNCTUATION}])")
}

fn autocomplete_separator_source() -> String {
    format!("(?:[{JS_WHITESPACE_CLASS}]|{})", cjk_punctuation_source())
}

/// Matches any grapheme from Han, Hiragana, Katakana, Hangul, or Bopomofo (by script extensions).
pub static CJK_BREAK_REGEX: LazyLock<Regex> = LazyLock::new(|| compile(&format!("[{CJK_CLASS}]")));

/// Matches CJK punctuation; CJK letters stay part of words and paths.
pub static CJK_PUNCTUATION_REGEX: LazyLock<Regex> = LazyLock::new(|| compile(&cjk_punctuation_source()));

/// Matches whitespace or CJK punctuation that separates an autocomplete token from preceding text.
pub static AUTOCOMPLETE_SEPARATOR_REGEX: LazyLock<Regex> = LazyLock::new(|| compile(&autocomplete_separator_source()));

/// Matches the start of input or an autocomplete separator.
pub static AUTOCOMPLETE_BOUNDARY_REGEX: LazyLock<Regex> =
    LazyLock::new(|| compile(&format!("(?:^|{})", autocomplete_separator_source())));

/// Matches one ASCII punctuation character used as a word navigation boundary.
pub static PUNCTUATION_REGEX: LazyLock<Regex> = LazyLock::new(|| compile(r#"[(){}\[\]<>.,;:'"!?+\-=*/\\|&%^$#@~`]"#));

/// Returns true when the string contains a whitespace character (JavaScript `/\s/.test`).
pub fn is_whitespace_char(c: &str) -> bool {
    c.chars().any(is_js_whitespace)
}

/// Returns true when the string contains an ASCII punctuation character from [`PUNCTUATION_REGEX`].
pub fn is_punctuation_char(c: &str) -> bool {
    PUNCTUATION_REGEX.is_match(c)
}

/// Whitespace as JavaScript defines it for `\s`, `trim`, and `trimEnd`.
pub(crate) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// JavaScript `String.prototype.trimEnd`.
pub(crate) fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// JavaScript `String.prototype.trim`.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// True when every byte is printable ASCII (0x20..=0x7E).
pub(crate) fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}
