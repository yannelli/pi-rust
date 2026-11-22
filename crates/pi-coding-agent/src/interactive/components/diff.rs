//! Colored diffs with intra-line highlighting (upstream `components/diff.ts`). Input lines look
//! like `+12 text`, `-12 text`, ` 12 text`, or `     ...`, as the edit tool writes them.

use pi_durable::diff::ChangeKind;

use super::word_diff::{diff_words, leading_whitespace};
use crate::theme::{ThemeColor, theme};

struct DiffLine<'a> {
    prefix: char,
    line_num: &'a str,
    content: &'a str,
}

/// Upstream `/^([+-\s])(\s*\d*)\s(.*)$/`.
fn parse_diff_line(line: &str) -> Option<DiffLine<'_>> {
    let prefix = line.chars().next()?;
    if !(prefix == '+' || prefix == '-' || prefix.is_whitespace()) {
        return None;
    }
    let rest = &line[prefix.len_utf8()..];
    let spaces = rest.len() - rest.trim_start_matches(char::is_whitespace).len();
    let after_spaces = &rest[spaces..];
    let digits = after_spaces.len() - after_spaces.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    // The number group is greedy, but the separator after it must be whitespace.
    let mut number_end = spaces + digits;
    loop {
        let separator = rest[number_end..].chars().next();
        if separator.is_some_and(char::is_whitespace) {
            let separator_len = separator.map_or(0, char::len_utf8);
            return Some(DiffLine {
                prefix,
                line_num: &rest[..number_end],
                content: &rest[number_end + separator_len..],
            });
        }
        if digits > 0 || number_end == 0 {
            return None;
        }
        // Backtrack into the leading whitespace when there are no digits.
        number_end -= rest[..number_end].chars().next_back().map_or(1, char::len_utf8);
    }
}

fn replace_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

fn split_leading_whitespace(value: &str) -> (&str, &str) {
    let leading = leading_whitespace(value);
    (leading, &value[leading.len()..])
}

/// Word-level diff of one changed line, with the changed words shown inverted.
fn render_intra_line_diff(old: &str, new: &str) -> (String, String) {
    let theme = theme();
    let (mut removed, mut added) = (String::new(), String::new());
    let (mut first_removed, mut first_added) = (true, true);
    for (kind, value) in diff_words(old, new) {
        let (line, first) = match kind {
            ChangeKind::Removed => (&mut removed, &mut first_removed),
            ChangeKind::Added => (&mut added, &mut first_added),
            ChangeKind::Equal => {
                removed.push_str(&value);
                added.push_str(&value);
                continue;
            }
        };
        let mut value = value.as_str();
        if *first {
            let (leading, rest) = split_leading_whitespace(value);
            line.push_str(leading);
            value = rest;
            *first = false;
        }
        if !value.is_empty() {
            line.push_str(&theme.inverse(value));
        }
    }
    (removed, added)
}

/// Render a diff: context lines dim, removed lines red, added lines green, and a single changed
/// line with its changed words inverted.
pub fn render_diff(diff_text: &str) -> String {
    let theme = theme();
    let lines: Vec<&str> = diff_text.split('\n').collect();
    let mut result = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(parsed) = parse_diff_line(line) else {
            result.push(theme.fg(ThemeColor::ToolDiffContext, line));
            i += 1;
            continue;
        };
        match parsed.prefix {
            '-' => {
                let mut removed = Vec::new();
                while let Some(p) = lines.get(i).and_then(|line| parse_diff_line(line)).filter(|p| p.prefix == '-') {
                    removed.push(p);
                    i += 1;
                }
                let mut added = Vec::new();
                while let Some(p) = lines.get(i).and_then(|line| parse_diff_line(line)).filter(|p| p.prefix == '+') {
                    added.push(p);
                    i += 1;
                }
                if removed.len() == 1 && added.len() == 1 {
                    let (old_line, new_line) =
                        render_intra_line_diff(&replace_tabs(removed[0].content), &replace_tabs(added[0].content));
                    result.push(theme.fg(ThemeColor::ToolDiffRemoved, &format!("-{} {old_line}", removed[0].line_num)));
                    result.push(theme.fg(ThemeColor::ToolDiffAdded, &format!("+{} {new_line}", added[0].line_num)));
                } else {
                    for line in removed {
                        let text = format!("-{} {}", line.line_num, replace_tabs(line.content));
                        result.push(theme.fg(ThemeColor::ToolDiffRemoved, &text));
                    }
                    for line in added {
                        let text = format!("+{} {}", line.line_num, replace_tabs(line.content));
                        result.push(theme.fg(ThemeColor::ToolDiffAdded, &text));
                    }
                }
            }
            '+' => {
                let text = format!("+{} {}", parsed.line_num, replace_tabs(parsed.content));
                result.push(theme.fg(ThemeColor::ToolDiffAdded, &text));
                i += 1;
            }
            _ => {
                let text = format!(" {} {}", parsed.line_num, replace_tabs(parsed.content));
                result.push(theme.fg(ThemeColor::ToolDiffContext, &text));
                i += 1;
            }
        }
    }
    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> String {
        pi_tui::strip_terminal_sequences(text)
    }

    #[test]
    fn parses_numbered_and_context_lines() {
        let line = parse_diff_line("+12 let x = 1;").unwrap();
        assert_eq!((line.prefix, line.line_num, line.content), ('+', "12", "let x = 1;"));
        let line = parse_diff_line("    ...").unwrap();
        assert_eq!((line.prefix, line.content), (' ', "..."));
        let line = parse_diff_line("- 7 old").unwrap();
        assert_eq!((line.line_num, line.content), (" 7", "old"));
        assert!(parse_diff_line("@@ hunk").is_none());
    }

    #[test]
    fn renders_single_line_changes_with_inverse_words() {
        let rendered = render_diff("-1 let value = 1;\n+1 let value = 2;\n 2 done");
        let lines: Vec<String> = rendered.split('\n').map(plain).collect();
        assert_eq!(lines, ["-1 let value = 1;", "+1 let value = 2;", " 2 done"]);
        assert!(rendered.contains("\x1b[7m"), "changed words are inverted: {rendered:?}");
    }

    #[test]
    fn renders_blocks_without_intra_line_diff() {
        let rendered = render_diff("-1 a\n-2 b\n+1 c");
        let lines: Vec<String> = rendered.split('\n').map(plain).collect();
        assert_eq!(lines, ["-1 a", "-2 b", "+1 c"]);
        assert!(!rendered.contains("\x1b[7m"));
    }
}
