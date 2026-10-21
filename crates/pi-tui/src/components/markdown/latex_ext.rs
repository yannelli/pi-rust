//! The `latex` and `latexBlock` tokenizer extensions of the upstream Markdown parser.

use std::sync::LazyLock;

use regex::Regex;

use super::token::Token;
use crate::utils::is_js_whitespace;

fn plain(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| panic!("invalid latex rule {pattern:?}: {err}"))
}

static PENDING_DOLLAR_MATH: LazyLock<Regex> = LazyLock::new(|| plain(r"\\[A-Za-z]+|[_^=+*/<>()\[\]|±≤≥≠≈∈→⇒∞∫∑√-]"));
static SHELL_CONSTANT: LazyLock<Regex> = LazyLock::new(|| plain(r"^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$"));
static IDENTIFIER_START: LazyLock<Regex> = LazyLock::new(|| plain(r"^[A-Za-z_][A-Za-z0-9_]*"));
static DOLLAR_BLOCK: LazyLock<Regex> = LazyLock::new(|| plain(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)"));
static BRACKET_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| plain(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)"));
static PENDING_BRACKET_BLOCK: LazyLock<Regex> = LazyLock::new(|| plain(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*)$"));
static PENDING_DOLLAR_BLOCK: LazyLock<Regex> = LazyLock::new(|| plain(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*)$"));
static BLOCK_START: LazyLock<Regex> = LazyLock::new(|| plain(r"(?:^|\n) {0,3}(?:\$\$|\\\[)"));
static TABLE_DELIMITER_ROW: LazyLock<Regex> =
    LazyLock::new(|| plain(r"^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$"));

fn is_escaped(source: &str, index: usize) -> bool {
    source[..index].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1
}

fn find_closing_delimiter(source: &str, closing: &str, start: usize) -> Option<usize> {
    let mut from = start;
    loop {
        let index = from + source.get(from..)?.find(closing)?;
        if !is_escaped(source, index) {
            return Some(index);
        }
        from = index + closing.len();
    }
}

fn looks_like_pending_dollar_math(source: &str) -> bool {
    PENDING_DOLLAR_MATH.is_match(source)
}

/// Whether `source` has a GFM table start: a header row followed by a delimiter row such as
/// `|---|---|` or `| --- |`. The header needs a `|` only when the delimiter has none: a pipe-free
/// delimiter without a pipe in the header is a thematic break, not a table.
fn contains_table_start(source: &str) -> bool {
    let lines: Vec<&str> = source.split('\n').collect();
    lines
        .windows(2)
        .any(|pair| (pair[1].contains('|') || pair[0].contains('|')) && TABLE_DELIMITER_ROW.is_match(pair[1]))
}

/// Upstream `tokenizeInlineLatex`: `$...$`, `$$...$$`, `\(...\)`, and `\[...\]` on one line.
pub(crate) fn tokenize_inline(source: &str) -> Option<Token> {
    let (opening, closing) = if source.starts_with("$$") {
        ("$$", "$$")
    } else if source.starts_with("\\(") {
        ("\\(", "\\)")
    } else if source.starts_with("\\[") {
        ("\\[", "\\]")
    } else if source.starts_with('$') && !source[1..].chars().next().is_some_and(is_js_whitespace) {
        ("$", "$")
    } else {
        return None;
    };

    let closing_index = find_closing_delimiter(source, closing, opening.len());
    if let Some(index) = closing_index
        && opening == "$"
    {
        let content = &source[opening.len()..index];
        let after = &source[index + 1..];
        let rejected = content.chars().next_back().is_some_and(is_js_whitespace)
            || after.starts_with(|c: char| c.is_ascii_digit())
            || (SHELL_CONSTANT.is_match(content) && IDENTIFIER_START.is_match(after))
            || content.contains('`');
        if rejected {
            return None;
        }
    }

    let Some(closing_index) = closing_index else {
        let pending = &source[opening.len()..];
        if opening.starts_with('\\') || looks_like_pending_dollar_math(pending) {
            return Some(Token::Latex { raw: source.to_string(), text: pending.to_string(), pending: true });
        }
        return None;
    };
    let text = &source[opening.len()..closing_index];
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    let raw = source[..closing_index + closing.len()].to_string();
    Some(Token::Latex { raw, text: text.to_string(), pending: false })
}

/// Upstream `tokenizeBlockLatex`: `$$...$$` and `\[...\]` blocks, possibly still streaming.
/// Display math never spans a table: without that check an unclosed `$$` or `\[` pairs with a
/// later opener or stays pending, swallowing the table.
pub(crate) fn tokenize_block(source: &str) -> Option<Token> {
    for regex in [&*DOLLAR_BLOCK, &*BRACKET_BLOCK] {
        if let Some(captures) = regex.captures(source)
            && !captures[1].is_empty()
            && !contains_table_start(&captures[1])
        {
            let raw = captures[0].to_string();
            let text = crate::utils::js_trim(&captures[1]).to_string();
            return Some(Token::LatexBlock { raw, text, pending: false });
        }
    }
    if let Some(captures) = PENDING_BRACKET_BLOCK.captures(source)
        && !captures[1].is_empty()
        && !contains_table_start(&captures[1])
    {
        return Some(Token::LatexBlock { raw: captures[0].to_string(), text: captures[1].to_string(), pending: true });
    }
    if let Some(captures) = PENDING_DOLLAR_BLOCK.captures(source)
        && !captures[1].is_empty()
        && looks_like_pending_dollar_math(&captures[1])
        && !contains_table_start(&captures[1])
    {
        return Some(Token::LatexBlock { raw: captures[0].to_string(), text: captures[1].to_string(), pending: true });
    }
    None
}

/// Where a math block may start in `source` (the upstream extension's `start`). With
/// `after_split_char`, `source` follows half of a UTF-16 surrogate pair in JavaScript, so a
/// match anchored at the start of the string is not possible.
pub(crate) fn block_start(source: &str, after_split_char: bool) -> Option<usize> {
    BLOCK_START
        .find_iter(source)
        .find(|found| !(after_split_char && found.start() == 0 && !found.as_str().starts_with('\n')))
        .map(|found| found.start() + usize::from(found.as_str().starts_with('\n')))
}

/// Where inline math may start in `source`.
pub(crate) fn inline_start(source: &str) -> Option<usize> {
    [source.find('$'), source.find("\\("), source.find("\\[")].into_iter().flatten().min()
}
