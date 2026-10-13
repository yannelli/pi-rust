//! Label cleanup, entity decoding, wrapping and truncation (upstream `labels.ts`).

use super::strings::{trim, words};
use super::width::{measured, string_width};

/// Node labels wrap to at most this many display columns per line ...
pub(super) const WRAP_WIDTH: usize = 24;
/// ... and at most this many lines; overflow is truncated with an ellipsis.
pub(super) const MAX_LINES: usize = 4;
/// Edge labels are truncated to this many columns.
pub(super) const MAX_LABEL: usize = 28;
/// Marks truncated text.
pub(super) const ELLIPSIS: char = '\u{2026}';

/// Identifier-boundary characters preferred as break points inside one long word.
const LABEL_BREAK_CHARS: [char; 4] = ['_', '-', '.', '/'];

/// Matches Rust's `char::is_alphanumeric` and the JS `[\p{Alphabetic}\p{N}]` class.
pub(super) fn is_alphanumeric(c: char) -> bool {
    c.is_alphanumeric()
}

/// Characters allowed in a bare node, state or class identifier.
pub(super) fn is_id_char(c: char) -> bool {
    is_alphanumeric(c) || c == '_'
}

const ENTITY_LOOKAHEAD: usize = 10;

fn decode_entity_body(body: &str) -> Option<String> {
    let named = match body {
        "lt" => Some("<"),
        "gt" => Some(">"),
        "amp" => Some("&"),
        "quot" => Some("\""),
        "apos" => Some("'"),
        // Upstream looks names up on a plain JS object, so inherited members decode too.
        "toString" => Some("function toString() { [native code] }"),
        "valueOf" => Some("function valueOf() { [native code] }"),
        "__proto__" => Some("[object Object]"),
        _ => None,
    };
    if let Some(named) = named {
        return Some(named.to_string());
    }
    let num = body.strip_prefix('#')?;
    let (digits, radix) = match num.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (num, 10),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return None;
    }
    let code = u64::from_str_radix(digits, radix).ok()?;
    // Surrogates, out-of-range values and controls never decode.
    if code > 0x10ffff || (0xd800..=0xdfff).contains(&code) || code < 0x20 || (0x7f..=0x9f).contains(&code) {
        return None;
    }
    char::from_u32(code as u32).map(String::from)
}

/// Decode HTML entities in label text in one pass, so `&amp;lt;` becomes `&lt;`.
pub(super) fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let hi = (i + 1 + ENTITY_LOOKAHEAD).min(chars.len());
        let semi = (i + 1..hi).find(|&j| chars[j] == ';');
        let decoded = semi.and_then(|semi| decode_entity_body(&chars[i + 1..semi].iter().collect::<String>()));
        match (decoded, semi) {
            (Some(text), Some(semi)) => {
                out.push_str(&text);
                i = semi + 1;
            }
            _ => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// Strip markdown emphasis from a `` `backtick` `` label string.
fn strip_markdown(s: &str) -> String {
    let no_code: String = s.chars().filter(|&c| c != '`').collect();
    let no_strong = no_code.replace("**", "").replace("__", "");
    let chars: Vec<char> = no_strong.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        // Keep `*` and `_` only inside a word, so snake_case survives.
        let in_word =
            i > 0 && is_alphanumeric(chars[i - 1]) && chars.get(i + 1).is_some_and(|&next| is_alphanumeric(next));
        if (c == '*' || c == '_') && !in_word {
            continue;
        }
        out.push(c);
    }
    trim(&out).to_string()
}

/// Inline formatting tags with no meaning in a terminal; other tag-like text stays.
const HTML_FORMAT_TAGS: [&str; 25] = [
    "b", "strong", "i", "em", "u", "s", "strike", "del", "ins", "mark", "small", "big", "sub", "sup", "code", "kbd",
    "samp", "var", "tt", "span", "font", "q", "abbr", "cite", "pre",
];

/// Read a tag starting at `start`, returning its name and the index after `>`.
fn html_tag_at(chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut i = start + 1;
    if chars.get(i) == Some(&'/') {
        i += 1;
    }
    let name_start = i;
    while i < chars.len() && chars[i].is_ascii_alphanumeric() {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();
    while i < chars.len() && chars[i] != '>' {
        if chars[i] == '<' {
            return None;
        }
        i += 1;
    }
    (chars.get(i) == Some(&'>')).then_some((name, i + 1))
}

fn strip_html_tags(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<'
            && let Some((name, end)) = html_tag_at(&chars, i)
        {
            let lower = name.to_ascii_lowercase();
            if lower == "br" {
                out.push(' ');
                i = end;
                continue;
            }
            if HTML_FORMAT_TAGS.contains(&lower.as_str()) {
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip one matching pair of wrapping delimiters, if present.
fn unwrap_pair(s: &str, delim: char) -> Option<&str> {
    if s.len() >= 2 && s.starts_with(delim) && s.ends_with(delim) { Some(&s[1..s.len() - 1]) } else { None }
}

/// Normalise raw label text: strip markup, unquote, then decode entities, so `<b>` is
/// removed as markup while `&lt;b&gt;` survives as the literal text `<b>`.
pub(super) fn clean_label(raw: &str) -> String {
    let stripped = strip_html_tags(trim(raw));
    let trimmed = trim(&stripped);
    let unquoted = trim(unwrap_pair(trimmed, '"').or_else(|| unwrap_pair(trimmed, '\'')).unwrap_or(trimmed));
    match unwrap_pair(unquoted, '`') {
        Some(md) => decode_html_entities(&strip_markdown(trim(md))),
        None => decode_html_entities(unquoted),
    }
}

/// Byte index of the last identifier-boundary character.
fn last_break(s: &str) -> Option<usize> {
    s.rfind(LABEL_BREAK_CHARS)
}

/// Wrap a label to `width` columns over at most `max_lines` lines, ending an
/// overflowing last line with an ellipsis. A word too wide to fit breaks after the
/// last identifier boundary (`_-./`) that fits, else per cluster.
pub(super) fn wrap_label(label: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0;

    for word in words(label) {
        let ww = string_width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for (ch, cw) in measured(word) {
                if chunk_w + cw > width && !chunk.is_empty() {
                    let (line, carry) = match last_break(&chunk) {
                        Some(p) => (chunk[..=p].to_string(), chunk[p + 1..].to_string()),
                        None => (chunk.clone(), String::new()),
                    };
                    lines.push(line);
                    chunk_w = string_width(&carry);
                    chunk = carry;
                }
                chunk.push_str(ch);
                chunk_w += cw;
            }
            cur = chunk;
            cur_w = chunk_w;
        } else if cur.is_empty() {
            cur = word.to_string();
            cur_w = ww;
        } else if cur_w + 1 + ww <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            lines.push(std::mem::replace(&mut cur, word.to_string()));
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        let target = width.saturating_sub(1).max(1);
        if let Some(last) = lines.last_mut() {
            let mut s = String::new();
            let mut sw = 0;
            for (ch, cw) in measured(last) {
                if sw + cw > target {
                    break;
                }
                s.push_str(ch);
                sw += cw;
            }
            s.push(ELLIPSIS);
            *last = s;
        }
    }
    lines
}

/// Truncate to `inner` columns, leaving room for the ellipsis.
pub(super) fn fit_label(label: &str, inner: usize) -> String {
    if string_width(label) <= inner {
        return label.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for (c, cw) in measured(label) {
        if used + cw + 1 > inner {
            break;
        }
        out.push_str(c);
        used += cw;
    }
    out.push(ELLIPSIS);
    out
}
