//! Lexical helpers for the export scanner: space and comments, `{...}` lists, and regex literals.

use super::scan::{Lang, group_after, is_ident_byte, matching_close, skip_lexeme};

pub fn skip_space(src: &str, mut i: usize) -> usize {
    let b = src.as_bytes();
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'/' && matches!(b.get(i + 1), Some(b'/' | b'*')) {
            i = skip_lexeme(src, i, Lang::Ts).unwrap_or(b.len());
        } else {
            break;
        }
    }
    i
}

/// The text of the `{...}` group at `at` without comments, and the index past it.
pub fn braces(src: &str, at: usize) -> Option<(String, usize)> {
    let close = matching_close(src, at, Lang::Ts)?;
    let body = group_after(src, at, b'{', Lang::Ts)?;
    let (mut out, mut i) = (String::new(), 0);
    while i < body.len() {
        let end = skip_lexeme(body, i, Lang::Ts).unwrap_or(i + body[i..].chars().next().map_or(1, char::len_utf8));
        out.push_str(if body.as_bytes()[i] == b'/' && end > i + 1 { " " } else { &body[i..end] });
        i = end;
    }
    Some((out, close + 1))
}

/// The index past a regular expression literal at `i`, judged by the code before it.
pub fn skip_regex(src: &str, i: usize) -> Option<usize> {
    let b = src.as_bytes();
    let before = src[..i].trim_end();
    let operand = before.bytes().last().is_some_and(|byte| is_ident_byte(byte) || matches!(byte, b')' | b']' | b'}'));
    let last = before.rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$').next().unwrap_or("");
    let keyword = ["return", "typeof", "case", "do", "else", "in", "of"].contains(&last);
    if b[i] != b'/' || matches!(b.get(i + 1), Some(b'/' | b'*')) || operand && !keyword {
        return None;
    }
    let (mut j, mut class) = (i + 1, false);
    while j < b.len() && b[j] != b'\n' {
        match b[j] {
            b'\\' => j += 1,
            b'[' => class = true,
            b']' => class = false,
            b'/' if !class => return Some(j + 1),
            _ => {}
        }
        j += 1;
    }
    None
}

/// The index past the string, comment, template, or regex at `i`; template expressions may hold regexes.
pub fn skip_code(src: &str, i: usize) -> Option<usize> {
    if src.as_bytes()[i] == b'`' {
        return Some(skip_template(src, i));
    }
    skip_lexeme(src, i, Lang::Ts).or_else(|| skip_regex(src, i))
}

fn skip_template(src: &str, start: usize) -> usize {
    let b = src.as_bytes();
    let mut j = start + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'`' => return j + 1,
            b'$' if b.get(j + 1) == Some(&b'{') => {
                let (mut depth, mut k) = (1usize, j + 2);
                while k < b.len() && depth > 0 {
                    if let Some(end) = skip_code(src, k) {
                        k = end;
                        continue;
                    }
                    match b[k] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    k += 1;
                }
                j = k;
            }
            _ => j += 1,
        }
    }
    b.len()
}
