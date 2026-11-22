//! jsdiff `diffWords`, which upstream uses for the word highlights of its diff view. Whitespace is
//! JavaScript `\s`.

use super::{ChangeKind, diff_tokens};

pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn is_word_char(c: char) -> bool {
    matches!(
        c,
        'a'..='z'
            | 'A'..='Z'
            | '0'..='9'
            | '_'
            | '\u{ad}'
            | '\u{c0}'..='\u{d6}'
            | '\u{d8}'..='\u{f6}'
            | '\u{f8}'..='\u{2c6}'
            | '\u{2c8}'..='\u{2d7}'
            | '\u{2de}'..='\u{2ff}'
            | '\u{1e00}'..='\u{1eff}'
    )
}

pub fn leading_whitespace(text: &str) -> &str {
    &text[..text.len() - text.trim_start_matches(is_js_whitespace).len()]
}

fn trailing_whitespace(text: &str) -> &str {
    &text[text.trim_end_matches(is_js_whitespace).len()..]
}

/// Byte ranges of the matches of the jsdiff regex `[word]+|\s+|[^word]`.
fn parts(text: &str) -> Vec<(usize, usize)> {
    let mut parts = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        let mut end = start + c.len_utf8();
        let same_class: Option<fn(char) -> bool> = if is_word_char(c) {
            Some(is_word_char)
        } else if is_js_whitespace(c) {
            Some(is_js_whitespace)
        } else {
            None
        };
        if let Some(same_class) = same_class {
            while let Some((i, next)) = chars.next_if(|&(_, next)| same_class(next)) {
                end = i + next.len_utf8();
            }
        }
        parts.push((start, end));
    }
    parts
}

/// jsdiff word tokens: each word or punctuation mark with the whitespace on both sides, so
/// adjacent tokens share their whitespace.
fn tokenize(text: &str) -> Vec<(usize, usize)> {
    let is_space = |(start, end): (usize, usize)| text[start..end].starts_with(is_js_whitespace);
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut prev = None;
    for part in parts(text) {
        if is_space(part) {
            match tokens.last_mut().filter(|_| prev.is_some()) {
                Some(last) => last.1 = part.1,
                None => tokens.push(part),
            }
        } else if let Some(space) = prev.filter(|&prev| is_space(prev)) {
            match tokens.last_mut().filter(|last| **last == space) {
                Some(last) => last.1 = part.1,
                None => tokens.push((space.0, part.1)),
            }
        } else {
            tokens.push(part);
        }
        prev = Some(part);
    }
    tokens
}

/// Tokens compare by their text without the surrounding whitespace.
fn words<'a>(text: &'a str, tokens: &[(usize, usize)]) -> Vec<&'a str> {
    tokens.iter().map(|&(start, end)| text[start..end].trim_matches(is_js_whitespace)).collect()
}

/// Word diff of one line: each change with its text, after the whitespace cleanup of jsdiff.
pub fn diff_words(old: &str, new: &str) -> Vec<(ChangeKind, String)> {
    let (old_tokens, new_tokens) = (tokenize(old), tokenize(new));
    let (old_words, new_words) = (words(old, &old_tokens), words(new, &new_tokens));
    let mut changes: Vec<(ChangeKind, String)> = diff_tokens(&old_words, &new_words, |a, b| a == b)
        .into_iter()
        .map(|change| {
            let (text, tokens) =
                if change.kind == ChangeKind::Removed { (old, &old_tokens) } else { (new, &new_tokens) };
            (change.kind, text[tokens[change.start].0..tokens[change.start + change.len - 1].1].to_owned())
        })
        .collect();
    let (mut last_keep, mut insertion, mut deletion) = (None, None, None);
    for i in 0..changes.len() {
        match changes[i].0 {
            ChangeKind::Added => insertion = Some(i),
            ChangeKind::Removed => deletion = Some(i),
            ChangeKind::Equal => {
                if insertion.is_some() || deletion.is_some() {
                    dedupe_whitespace(&mut changes, last_keep, deletion, insertion, Some(i));
                }
                (last_keep, insertion, deletion) = (Some(i), None, None);
            }
        }
    }
    if insertion.is_some() || deletion.is_some() {
        dedupe_whitespace(&mut changes, last_keep, deletion, insertion, None);
    }
    changes
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let len = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    &a[..len]
}

fn common_suffix<'a>(a: &'a str, b: &str) -> &'a str {
    let len: usize = a.chars().rev().zip(b.chars().rev()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    &a[a.len() - len..]
}

// jsdiff throws when the text lacks the prefix or suffix; these keep the text as it is.
fn replace_prefix(text: &str, old: &str, new: &str) -> String {
    text.strip_prefix(old).map_or_else(|| text.to_owned(), |rest| format!("{new}{rest}"))
}

fn replace_suffix(text: &str, old: &str, new: &str) -> String {
    text.strip_suffix(old).map_or_else(|| text.to_owned(), |rest| format!("{rest}{new}"))
}

/// jsdiff `maximumOverlap`, a KMP search: the longest prefix of `b` that `a` ends with.
fn maximum_overlap<'b>(a: &str, b: &'b str) -> &'b str {
    let (a, b_chars): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let start_a = a.len().saturating_sub(b_chars.len());
    let end_b = b_chars.len().min(a.len());
    let mut map = vec![0; end_b];
    let mut k = 0;
    for j in 1..end_b {
        map[j] = if b_chars[j] == b_chars[k] { map[k] } else { k };
        while k > 0 && b_chars[j] != b_chars[k] {
            k = map[k];
        }
        if b_chars[j] == b_chars[k] {
            k += 1;
        }
    }
    k = 0;
    for &c in &a[start_a..] {
        while k > 0 && c != b_chars[k] {
            k = map[k];
        }
        if b_chars.get(k) == Some(&c) {
            k += 1;
        }
    }
    let len: usize = b_chars[..k].iter().map(|c| c.len_utf8()).sum();
    &b[..len]
}

/// jsdiff `dedupeWhitespaceInChangeObjects`: whitespace that two neighboring changes share is
/// kept once.
fn dedupe_whitespace(
    changes: &mut [(ChangeKind, String)],
    start_keep: Option<usize>,
    deletion: Option<usize>,
    insertion: Option<usize>,
    end_keep: Option<usize>,
) {
    match (deletion, insertion) {
        (Some(deletion), Some(insertion)) => {
            let (old_value, new_value) = (changes[deletion].1.clone(), changes[insertion].1.clone());
            let (old_prefix, old_suffix) = (leading_whitespace(&old_value), trailing_whitespace(&old_value));
            let (new_prefix, new_suffix) = (leading_whitespace(&new_value), trailing_whitespace(&new_value));
            if let Some(keep) = start_keep {
                let common = common_prefix(old_prefix, new_prefix);
                changes[keep].1 = replace_suffix(&changes[keep].1, new_prefix, common);
                changes[deletion].1 = replace_prefix(&changes[deletion].1, common, "");
                changes[insertion].1 = replace_prefix(&changes[insertion].1, common, "");
            }
            if let Some(keep) = end_keep {
                let common = common_suffix(old_suffix, new_suffix);
                changes[keep].1 = replace_prefix(&changes[keep].1, new_suffix, common);
                changes[deletion].1 = replace_suffix(&changes[deletion].1, common, "");
                changes[insertion].1 = replace_suffix(&changes[insertion].1, common, "");
            }
        }
        (None, Some(insertion)) => {
            for i in [start_keep.map(|_| insertion), end_keep].into_iter().flatten() {
                changes[i].1 = changes[i].1[leading_whitespace(&changes[i].1).len()..].to_owned();
            }
        }
        (Some(deletion), None) => match (start_keep, end_keep) {
            (Some(start), Some(end)) => {
                let (end_value, deleted) = (changes[end].1.clone(), changes[deletion].1.clone());
                let new_full = leading_whitespace(&end_value);
                let new_start = common_prefix(new_full, leading_whitespace(&deleted));
                changes[deletion].1 = replace_prefix(&changes[deletion].1, new_start, "");
                let new_end = common_suffix(&new_full[new_start.len()..], trailing_whitespace(&deleted));
                changes[deletion].1 = replace_suffix(&changes[deletion].1, new_end, "");
                changes[end].1 = replace_prefix(&end_value, new_full, new_end);
                let kept = &new_full[..new_full.len() - new_end.len()];
                changes[start].1 = replace_suffix(&changes[start].1, new_full, kept);
            }
            (None, Some(end)) => {
                let overlap =
                    maximum_overlap(trailing_whitespace(&changes[deletion].1), leading_whitespace(&changes[end].1));
                changes[deletion].1 = replace_suffix(&changes[deletion].1, overlap, "");
            }
            (Some(start), None) => {
                let overlap =
                    maximum_overlap(trailing_whitespace(&changes[start].1), leading_whitespace(&changes[deletion].1));
                changes[deletion].1 = replace_prefix(&changes[deletion].1, overlap, "");
            }
            (None, None) => {}
        },
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ChangeKind::{Added, Equal, Removed};

    type Case = (&'static str, &'static str, &'static [(ChangeKind, &'static str)]);

    /// `(old, new, changes)`, as jsdiff 8.0.4 `diffWords` gives them.
    const CASES: &[Case] = &[
        ("call(a)", "call(b)", &[(Equal, "call("), (Removed, "a"), (Added, "b"), (Equal, ")")]),
        ("foo bar baz", "foo qux baz", &[(Equal, "foo "), (Removed, "bar"), (Added, "qux"), (Equal, " baz")]),
        ("foo bar baz", "foo baz", &[(Equal, "foo "), (Removed, "bar "), (Equal, "baz")]),
        ("foo   bar baz", "foo  baz", &[(Equal, "foo  "), (Removed, " bar "), (Equal, "baz")]),
        ("foo\nbar baz", "foo baz", &[(Equal, "foo"), (Removed, "\nbar"), (Equal, " baz")]),
        ("foo baz", "foo\nbar baz", &[(Equal, "foo\n"), (Added, "bar "), (Equal, "baz")]),
        (
            "  let value = 1;",
            "  let value = 2;",
            &[(Equal, "  let value = "), (Removed, "1"), (Added, "2"), (Equal, ";")],
        ),
        ("a\u{a0}b", "a\u{a0}c", &[(Equal, "a\u{a0}"), (Removed, "b"), (Added, "c")]),
        ("\u{feff}x y", "x y", &[(Equal, "x y")]),
        ("a\u{85}b", "a\u{85}c", &[(Equal, "a\u{85}"), (Removed, "b"), (Added, "c")]),
        (" ", "  ", &[(Equal, "  ")]),
        ("", "a", &[(Added, "a")]),
        ("a", "", &[(Removed, "a")]),
        ("foo bar", "bar", &[(Removed, "foo "), (Equal, "bar")]),
        ("foo bar", "foo", &[(Equal, "foo"), (Removed, " bar")]),
        ("foo \t bar", "foo \tbar baz", &[(Equal, "foo \tbar "), (Added, "baz")]),
        ("foo baz", "foo bar baz", &[(Equal, "foo "), (Added, "bar "), (Equal, "baz")]),
        ("bar", "foo bar", &[(Added, "foo "), (Equal, "bar")]),
        ("foo", "foo bar", &[(Equal, "foo "), (Added, "bar")]),
        ("foo\tbar baz", "foo baz", &[(Equal, "foo"), (Removed, "\tbar"), (Equal, " baz")]),
        (
            "a b c d",
            "a x c y",
            &[(Equal, "a "), (Removed, "b"), (Added, "x"), (Equal, " c "), (Removed, "d"), (Added, "y")],
        ),
        ("  x", "x  ", &[(Equal, "x  ")]),
        ("f(a, b)", "f(a,b)", &[(Equal, "f(a,b)")]),
        (
            "\u{e9}\u{4e2d}\u{1f600} x",
            "\u{e9}\u{4e2d} x",
            &[(Equal, "\u{e9}\u{4e2d}"), (Removed, "\u{1f600}"), (Equal, " x")],
        ),
    ];

    #[test]
    fn word_diffs_match_jsdiff() {
        for &(old, new, expected) in CASES {
            let expected: Vec<(ChangeKind, String)> =
                expected.iter().map(|&(kind, text)| (kind, text.to_owned())).collect();
            assert_eq!(diff_words(old, new), expected, "{old:?} -> {new:?}");
        }
    }
}
