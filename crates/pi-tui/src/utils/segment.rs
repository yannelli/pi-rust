//! Grapheme and word segmentation (the replacements for the shared `Intl.Segmenter` instances).

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

use super::width::is_extend_or_format;

/// One segment from [`word_segments`], like `Intl.SegmentData` for word granularity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WordSegment<'a> {
    /// The segment text, a slice of the input.
    pub segment: &'a str,
    /// Byte offset of the segment in the input.
    pub index: usize,
    /// True for words (letters, digits, ideographs, kana); false for spaces, punctuation, and symbols.
    pub is_word_like: bool,
}

/// Iterates over the extended grapheme clusters of `s`.
pub fn graphemes(s: &str) -> impl DoubleEndedIterator<Item = &str> {
    s.graphemes(true)
}

/// Characters that make a word segment word-like, tuned to match ICU's `isWordLike` per character.
static WORD_LIKE_CHAR: LazyLock<Regex> = LazyLock::new(|| {
    let pattern = concat!(
        r"[[\p{Alphabetic}\p{Nd}\x{B8}\x{2C2}-\x{2C5}\x{2D2}-\x{2D7}\x{2DE}\x{2DF}\x{2E5}-\x{2EB}\x{2ED}",
        r"\x{2EF}-\x{2FF}\x{55A}-\x{55C}\x{55E}\x{58A}\x{5F3}\x{A708}-\x{A716}\x{A720}\x{A721}\x{A789}\x{A78A}",
        r"\x{AB5B}\x{600}-\x{605}\x{66B}\x{6DD}\x{70F}\x{890}\x{891}\x{8E2}\x{309B}\x{309C}\x{30A0}\x{32D0}-\x{32FE}",
        r"\x{3300}-\x{3357}\x{110BD}\x{110CD}\x{1F200}]--[\x{3005}\x{303B}\x{16FE3}\x{16FF0}\x{16FF1}]]",
    );
    Regex::new(pattern).unwrap_or_else(|err| panic!("word-like regex: {err}"))
});

/// ICU reports no word status for a ZWJ emoji join or a connector followed by marks or format characters.
static ICU_NOT_WORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x{200D}\p{Extended_Pictographic}|[\p{Pc}\x{202F}][\p{M}\p{Cf}]+$")
        .unwrap_or_else(|err| panic!("not-word regex: {err}"))
});

/// ICU treats a run of two or more connectors (such as `__`) as a word.
static CONNECTOR_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{Pc}\x{202F}]{2,}$").unwrap_or_else(|err| panic!("connector regex: {err}")));

static EXTENDED_PICTOGRAPHIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\p{Extended_Pictographic}$").unwrap_or_else(|err| panic!("pictographic regex: {err}"))
});

fn is_word_like(segment: &str) -> bool {
    !ICU_NOT_WORD.is_match(segment) && (WORD_LIKE_CHAR.is_match(segment) || CONNECTOR_RUN.is_match(segment))
}

fn is_hangul_syllable(c: char) -> bool {
    ('\u{AC00}'..='\u{D7A3}').contains(&c)
}

fn push_uax29<'a>(piece: &'a str, offset: usize, out: &mut Vec<WordSegment<'a>>) {
    out.extend(piece.split_word_bound_indices().map(|(index, segment)| WordSegment {
        segment,
        index: offset + index,
        is_word_like: is_word_like(segment),
    }));
}

/// Splits `s` at word boundaries (`index` is a byte offset): UAX #29, plus ICU's rule that Hangul
/// syllable runs are words of their own (`API를` gives `API`, `를`). ICU's dictionary is absent, so
/// Han, kana, and Thai runs follow plain UAX #29 rules.
pub fn word_segments(s: &str) -> Vec<WordSegment<'_>> {
    let mut out = Vec::new();
    if !s.contains(is_hangul_syllable) {
        push_uax29(s, 0, &mut out);
        return out;
    }
    // Hangul runs alternate with other text. Marks and format characters join the run before them,
    // and a Hangul syllable after such a character starts a new run.
    let mut run_start = 0;
    let mut run_is_hangul: Option<bool> = None;
    let mut after_attached = false;
    let mut previous_char = '\0';
    for (i, c) in s.char_indices() {
        // A ZWJ emoji join (UAX #29 rule WB3c) also stays in the run.
        let zwj_join = previous_char == '\u{200D}' && EXTENDED_PICTOGRAPHIC.is_match(c.encode_utf8(&mut [0; 4]));
        previous_char = c;
        let attached = run_is_hangul.is_some() && (is_extend_or_format(c) || zwj_join);
        let is_hangul = if attached { run_is_hangul == Some(true) } else { is_hangul_syllable(c) };
        let restart = is_hangul && after_attached && !attached;
        if run_is_hangul.is_some_and(|previous| previous != is_hangul) || restart {
            push_run(s, run_start, i, run_is_hangul == Some(true), &mut out);
            run_start = i;
        }
        run_is_hangul = Some(is_hangul);
        after_attached = attached;
    }
    push_run(s, run_start, s.len(), run_is_hangul == Some(true), &mut out);
    out
}

fn push_run<'a>(s: &'a str, start: usize, end: usize, is_hangul: bool, out: &mut Vec<WordSegment<'a>>) {
    let piece = &s[start..end];
    if is_hangul {
        // ICU only marks the run word-like when it has no trailing marks.
        out.push(WordSegment { segment: piece, index: start, is_word_like: piece.chars().all(is_hangul_syllable) });
    } else {
        push_uax29(piece, start, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(s: &str) -> Vec<(&str, usize, bool)> {
        word_segments(s).into_iter().map(|w| (w.segment, w.index, w.is_word_like)).collect()
    }

    #[test]
    fn segments_words_like_icu_for_latin_text() {
        assert_eq!(
            summary("path/to/file"),
            [("path", 0, true), ("/", 4, false), ("to", 5, true), ("/", 7, false), ("file", 8, true)]
        );
        assert_eq!(summary("foo.bar"), [("foo.bar", 0, true)]);
        assert_eq!(summary("  hello  "), [("  ", 0, false), ("hello", 2, true), ("  ", 7, false)]);
        assert_eq!(summary("a_b _"), [("a_b", 0, true), (" ", 3, false), ("_", 4, false)]);
        assert_eq!(
            summary("x\u{B2} 3,000"),
            [("x", 0, true), ("\u{B2}", 1, false), (" ", 3, false), ("3,000", 4, true)]
        );
    }

    #[test]
    fn copies_icu_word_status_quirks() {
        assert_eq!(summary("__"), [("__", 0, true)]);
        assert_eq!(summary("a_\u{200D}"), [("a_\u{200D}", 0, false)]);
        assert_eq!(summary("a_\u{200D}b"), [("a_\u{200D}b", 0, true)]);
        assert_eq!(summary("a\u{200D}\u{1F642}"), [("a\u{200D}\u{1F642}", 0, false)]);
        assert_eq!(summary("a\u{200D}"), [("a\u{200D}", 0, true)]);
    }

    #[test]
    fn splits_hangul_runs_like_icu() {
        assert_eq!(
            summary("API\u{B97C} \u{C0AC}\u{C6A9}"),
            [("API", 0, true), ("\u{B97C}", 3, true), (" ", 6, false), ("\u{C0AC}\u{C6A9}", 7, true)]
        );
        assert_eq!(summary("\u{AC00}'a"), [("\u{AC00}", 0, true), ("'", 3, false), ("a", 4, true)]);
        assert_eq!(summary("a_\u{AC00}\u{301}x"), [("a_", 0, true), ("\u{AC00}\u{301}", 2, false), ("x", 7, true)]);
        assert_eq!(summary("\u{3131}a"), [("\u{3131}a", 0, true)]);
        assert_eq!(summary("\u{AC00}\u{200D}\u{AC00}"), [("\u{AC00}\u{200D}", 0, false), ("\u{AC00}", 6, true)]);
        assert_eq!(summary("\u{AC00}\u{200D}\u{1F642}a"), [("\u{AC00}\u{200D}\u{1F642}", 0, false), ("a", 10, true)]);
    }

    #[test]
    fn iterates_graphemes() {
        let parts: Vec<&str> = graphemes("e\u{301}\u{1F1EF}\u{1F1F5}a").collect();
        assert_eq!(parts, ["e\u{301}", "\u{1F1EF}\u{1F1F5}", "a"]);
        assert_eq!(graphemes("ab").next_back(), Some("b"));
    }
}
