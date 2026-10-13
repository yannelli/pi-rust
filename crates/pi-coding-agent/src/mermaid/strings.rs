//! String helpers that follow JavaScript semantics, so trimming and word splitting
//! agree with upstream: JS `\s` and `trim()` include U+FEFF but not U+0085.

/// JavaScript `WhiteSpace` and `LineTerminator`, the set `\s` and `trim()` use.
pub(super) fn is_js_space(c: char) -> bool {
    const OTHER: [char; 9] =
        ['\u{a0}', '\u{1680}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}', '\u{feff}', ' '];
    matches!(c, '\t'..='\r' | '\u{2000}'..='\u{200a}') || OTHER.contains(&c)
}

pub(super) fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

pub(super) fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_js_space)
}

pub(super) fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_space)
}

/// `s.split(/\s+/).filter((w) => w !== '')`.
pub(super) fn words(s: &str) -> Vec<&str> {
    s.split(is_js_space).filter(|w| !w.is_empty()).collect()
}

/// The first whitespace-separated word, or `""`.
pub(super) fn first_word(s: &str) -> &str {
    s.split(is_js_space).find(|w| !w.is_empty()).unwrap_or("")
}

/// `/\s/.test(s)`.
pub(super) fn has_space(s: &str) -> bool {
    s.chars().any(is_js_space)
}

/// C0 and C1 controls other than `\t`, `\n` and `\r`: they measure one column but
/// paint none, and ESC would inject ANSI into the caller's scrollback.
fn is_stripped_control(c: char) -> bool {
    matches!(c, '\0'..='\u{08}' | '\u{0b}' | '\u{0c}' | '\u{0e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// Applied by every public entry point that takes untrusted source.
pub(super) fn strip_controls(src: &str) -> String {
    src.chars().filter(|&c| !is_stripped_control(c)).collect()
}

/// Split like Rust's `str::lines()`: on `\n`, dropping one trailing `\r` per line and
/// the empty element after a final newline.
pub(super) fn src_lines(src: &str) -> Vec<&str> {
    let mut out: Vec<&str> = src.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect();
    if out.last() == Some(&"") {
        out.pop();
    }
    out
}

/// Strip every trailing occurrence of `c`, like `.replace(/c+$/, '')`.
pub(super) fn strip_trailing(s: &str, c: char) -> &str {
    s.trim_end_matches(c)
}

/// Keep `s` unless it is empty.
pub(super) fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}
