//! Grapheme and word segmentation (the replacements for the shared `Intl.Segmenter` instances).

use std::sync::LazyLock;

use icu_segmenter::WordSegmenter;
use icu_segmenter::options::WordBreakInvariantOptions;
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

/// A run of characters that ICU segments with a dictionary (Han, kana, Thai, Lao, Khmer, Myanmar),
/// with the marks and joiners that follow them.
static DICTIONARY_RUN: LazyLock<Regex> = LazyLock::new(|| {
    let letter = r"\p{Han}\p{Hiragana}\p{Katakana}\p{Thai}\p{Lao}\p{Khmer}\p{Myanmar}\x{30FC}";
    Regex::new(&format!(r"[{letter}][{letter}\p{{M}}\x{{200C}}\x{{200D}}]*"))
        .unwrap_or_else(|err| panic!("dictionary run regex: {err}"))
});

fn push_segment<'a>(segment: &'a str, index: usize, out: &mut Vec<WordSegment<'a>>) {
    out.push(WordSegment { segment, index, is_word_like: is_word_like(segment) });
}

/// True for pieces that ICU keeps on the word before them: marks, joiners, and the Thai and Lao
/// repetition and abbreviation signs.
fn attaches_to_previous(piece: &str) -> bool {
    piece.chars().all(|c| is_extend_or_format(c) || matches!(c, '\u{0E2F}' | '\u{0E46}' | '\u{0EC6}'))
}

/// Splits a run of dictionary-script text into dictionary words with ICU4X. Like ICU, every
/// dictionary word is word-like; a lone character keeps its own status (`々` alone is not).
fn push_dictionary_run<'a>(run: &'a str, offset: usize, out: &mut Vec<WordSegment<'a>>) {
    if run.chars().nth(1).is_none() {
        push_segment(run, offset, out);
        return;
    }
    let segmenter = WordSegmenter::new_dictionary(WordBreakInvariantOptions::default());
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    for end in segmenter.segment_str(run).filter(|&end| end > 0) {
        match words.last_mut() {
            Some(word) if attaches_to_previous(&run[start..end]) => word.1 = end,
            _ => words.push((start, end)),
        }
        start = end;
    }
    out.extend(words.into_iter().map(|(start, end)| WordSegment {
        segment: &run[start..end],
        index: offset + start,
        is_word_like: true,
    }));
}

/// UAX #29 word segments, with each run of dictionary-script text split into dictionary words.
fn push_uax29<'a>(piece: &'a str, offset: usize, out: &mut Vec<WordSegment<'a>>) {
    let mut last = 0;
    for run in DICTIONARY_RUN.find_iter(piece) {
        push_rule_based(&piece[last..run.start()], offset + last, out);
        push_dictionary_run(run.as_str(), offset + run.start(), out);
        last = run.end();
    }
    push_rule_based(&piece[last..], offset + last, out);
}

fn push_rule_based<'a>(piece: &'a str, offset: usize, out: &mut Vec<WordSegment<'a>>) {
    for (index, segment) in piece.split_word_bound_indices() {
        push_segment(segment, offset + index, out);
    }
}

/// Splits `s` at word boundaries (`index` is a byte offset): UAX #29, plus ICU's rule that Hangul
/// syllable runs are words of their own (`API를` gives `API`, `를`), and Han, kana, Thai, Lao, Khmer,
/// and Myanmar runs split into dictionary words (ICU4X's compiled dictionaries).
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

    fn words(s: &str) -> Vec<&str> {
        word_segments(s).into_iter().map(|w| w.segment).collect()
    }

    #[test]
    fn splits_dictionary_scripts_into_words() {
        assert_eq!(
            summary("你好世界 test"),
            [("你好", 0, true), ("世界", 6, true), (" ", 12, false), ("test", 13, true)]
        );
        assert_eq!(words("中华人民共和国"), ["中华", "人民", "共和国"]);
        assert_eq!(words("こんにちは世界"), ["こんにちは", "世界"]);
        assert_eq!(
            words("パソコンでプログラミングを勉強する"),
            ["パソコン", "で", "プログラミング", "を", "勉強", "する"]
        );
        assert_eq!(words("ฉันชอบกินข้าวผัด"), ["ฉัน", "ชอบ", "กิน", "ข้าว", "ผัด"]);
        assert_eq!(words("ทุกสองสัปดาห์"), ["ทุก", "สอง", "สัปดาห์"]);
    }

    #[test]
    fn keeps_icu_word_status_around_dictionary_words() {
        assert_eq!(summary("你好，世界"), [("你好", 0, true), ("\u{FF0C}", 6, false), ("世界", 9, true)]);
        assert_eq!(summary("ไปๆมาๆ"), [("ไปๆ", 0, true), ("มาๆ", 9, true)]);
        assert_eq!(summary("日本\u{200D}語"), [("日本\u{200D}", 0, true), ("語", 9, true)]);
        assert_eq!(summary("々a"), [("々", 0, false), ("a", 3, true)]);
        assert_eq!(words("hello你好，world世界"), ["hello", "你好", "\u{FF0C}", "world", "世界"]);
    }

    #[test]
    fn iterates_graphemes() {
        let parts: Vec<&str> = graphemes("e\u{301}\u{1F1EF}\u{1F1F5}a").collect();
        assert_eq!(parts, ["e\u{301}", "\u{1F1EF}\u{1F1F5}", "a"]);
        assert_eq!(graphemes("ab").next_back(), Some("b"));
    }
}
