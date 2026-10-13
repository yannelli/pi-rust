//! Word wrapping that keeps ANSI styling and OSC 8 links intact across line breaks.

use unicode_segmentation::UnicodeSegmentation;

use super::ansi::{ansi_code_length, extract_ansi_code, next_ansi_start};
use super::tracker::AnsiCodeTracker;
use super::width::visible_width;
use super::{CJK_BREAK_REGEX, is_printable_ascii, js_trim, js_trim_end};

/// Releases spare capacity of cached lines; the upstream version flattens V8 rope strings.
pub fn flatten_lines(lines: &mut Vec<String>) {
    for line in lines.iter_mut() {
        line.shrink_to_fit();
    }
    lines.shrink_to_fit();
}

/// Splits text into word, space, and CJK grapheme tokens; escape codes stay with the next visible text.
fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    #[derive(PartialEq, Eq, Clone, Copy)]
    enum Kind {
        Space,
        Word,
    }

    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_kind: Option<Kind> = None;
    let mut pending_ansi = String::new();
    let mut i = 0;

    while i < text.len() {
        if let Some(code) = extract_ansi_code(text, i) {
            pending_ansi.push_str(code);
            i += code.len();
            continue;
        }

        // Visible text runs up to the next escape sequence.
        let first_len = text[i..].chars().next().map_or(1, char::len_utf8);
        let end = next_ansi_start(text, i + first_len);
        let chunk = &text[i..end];
        // Printable ASCII characters are single graphemes, so skip the segmenter for them.
        let ascii = is_printable_ascii(chunk);
        let segments: Box<dyn Iterator<Item = &str>> =
            if ascii { Box::new((0..chunk.len()).map(|k| &chunk[k..k + 1])) } else { Box::new(chunk.graphemes(true)) };
        for segment in segments {
            let is_space = segment == " ";
            if !ascii && !is_space && CJK_BREAK_REGEX.is_match(segment) {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                current_kind = None;
                tokens.push(format!("{pending_ansi}{segment}"));
                pending_ansi.clear();
                continue;
            }

            let kind = if is_space { Kind::Space } else { Kind::Word };
            if !current.is_empty() && current_kind != Some(kind) {
                tokens.push(std::mem::take(&mut current));
            }
            current.push_str(&pending_ansi);
            pending_ansi.clear();
            current_kind = Some(kind);
            current.push_str(segment);
        }
        i = end;
    }

    // Trailing escape codes attach to the last token.
    if !pending_ansi.is_empty() {
        if !current.is_empty() {
            current.push_str(&pending_ansi);
        } else if let Some(last) = tokens.last_mut() {
            last.push_str(&pending_ansi);
        } else {
            current = pending_ansi;
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Splits on `\r\n`, `\r`, or `\n`.
fn split_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                lines.push(&text[start..i]);
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            b'\n' => {
                lines.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    lines.push(&text[start..]);
    lines
}

/// Word-wraps text to `width` visible cells and keeps ANSI styling across breaks and literal newlines.
/// It only wraps: lines are not padded and get no background.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    for input_line in split_lines(text) {
        // Lines after the first start with the codes still active from earlier lines.
        let prefix = if result.is_empty() { String::new() } else { tracker.get_active_codes() };
        result.extend(wrap_single_line(&format!("{prefix}{input_line}"), width));
        tracker.process_text(input_line);
    }
    if result.is_empty() { vec![String::new()] } else { result }
}

fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if visible_width(line) <= width {
        return vec![line.to_string()];
    }

    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiCodeTracker::new();
    let mut current_line = String::new();
    let mut current_width = 0;

    for token in split_into_tokens_with_ansi(line) {
        let token_width = visible_width(&token);
        let is_whitespace = js_trim(&token).is_empty();

        if token_width > width && !is_whitespace {
            if !current_line.is_empty() {
                // Underline-only reset keeps the background for padding.
                current_line.push_str(&tracker.get_line_end_reset());
                wrapped.push(std::mem::take(&mut current_line));
            }
            let mut broken = break_long_word(&token, width, &mut tracker);
            current_line = broken.pop().unwrap_or_default();
            wrapped.extend(broken);
            current_width = visible_width(&current_line);
            continue;
        }

        if current_width + token_width > width && current_width > 0 {
            let mut line_to_wrap = js_trim_end(&current_line).to_string();
            line_to_wrap.push_str(&tracker.get_line_end_reset());
            wrapped.push(line_to_wrap);
            if is_whitespace {
                // Do not start a new line with whitespace.
                current_line = tracker.get_active_codes();
                current_width = 0;
            } else {
                current_line = tracker.get_active_codes() + &token;
                current_width = token_width;
            }
        } else {
            current_line.push_str(&token);
            current_width += token_width;
        }
        tracker.process_text(&token);
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }
    // Trailing whitespace can push lines past the requested width.
    if wrapped.is_empty() {
        return vec![String::new()];
    }
    wrapped.into_iter().map(|line| js_trim_end(&line).to_string()).collect()
}

fn break_long_word(word: &str, width: usize, tracker: &mut AnsiCodeTracker) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_width = 0;
    let mut i = 0;

    while i < word.len() {
        let length = ansi_code_length(word, i);
        if length > 0 {
            let code = &word[i..i + length];
            current_line.push_str(code);
            tracker.process(code);
            i += length;
            continue;
        }
        let end = next_ansi_start(word, i);
        for grapheme in word[i..end].graphemes(true) {
            let grapheme_width = visible_width(grapheme);
            if current_width + grapheme_width > width {
                current_line.push_str(&tracker.get_line_end_reset());
                lines.push(std::mem::replace(&mut current_line, tracker.get_active_codes()));
                current_width = 0;
            }
            current_line.push_str(grapheme);
            current_width += grapheme_width;
        }
        i = end;
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }
    if lines.is_empty() { vec![String::new()] } else { lines }
}

/// Pads `line` with spaces to `width` cells and passes the result through the background function.
pub fn apply_background_to_line(line: &str, width: usize, bg: &dyn Fn(&str) -> String) -> String {
    let padding = width.saturating_sub(visible_width(line));
    bg(&format!("{line}{}", " ".repeat(padding)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_words_spaces_and_cjk() {
        assert_eq!(split_into_tokens_with_ansi("ab  cd"), ["ab", "  ", "cd"]);
        assert_eq!(split_into_tokens_with_ansi("x\x1b[31m中文\x1b[0m"), ["x", "\x1b[31m中", "文\x1b[0m"]);
        assert_eq!(split_into_tokens_with_ansi("\x1b[0m"), ["\x1b[0m"]);
    }

    #[test]
    fn splits_all_line_endings() {
        assert_eq!(split_lines("a\r\nb\rc\nd"), ["a", "b", "c", "d"]);
        assert_eq!(split_lines("a\n"), ["a", ""]);
    }

    #[test]
    fn breaks_long_words_and_applies_background() {
        assert_eq!(wrap_text_with_ansi("abcdefgh", 3), ["abc", "def", "gh"]);
        let bg = |s: &str| format!("<{s}>");
        assert_eq!(apply_background_to_line("ab", 4, &bg), "<ab  >");
        let mut lines = vec![String::with_capacity(64)];
        flatten_lines(&mut lines);
        assert!(lines[0].capacity() < 64);
    }
}
