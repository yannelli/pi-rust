//! Line diffs of the edit tool: upstream `generateDiffString` and jsdiff `createTwoFilesPatch` with
//! `FILE_HEADERS_ONLY`.

use std::fmt::Write as _;

use super::{Change, ChangeKind, diff_tokens};

/// The context lines of upstream `generateDiffString` and `generateUnifiedPatch` when the caller gives none.
pub const DEFAULT_CONTEXT_LINES: usize = 4;

/// The display diff, the first changed line of the new text, and the unified patch of one edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiff {
    pub diff: String,
    pub first_changed_line: Option<usize>,
    pub patch: String,
}

/// jsdiff line tokens: each line with its `\n` or `\r\n`.
pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

struct LineDiff<'a> {
    old: Vec<&'a str>,
    new: Vec<&'a str>,
    changes: Vec<Change>,
}

impl<'a> LineDiff<'a> {
    fn new(old: &'a str, new: &'a str) -> Self {
        let (old, new) = (split_lines(old), split_lines(new));
        let changes = diff_tokens(&old, &new, |a, b| a == b);
        Self { old, new, changes }
    }

    fn lines(&self, change: &Change) -> &[&'a str] {
        let tokens = if change.kind == ChangeKind::Removed { &self.old } else { &self.new };
        &tokens[change.start..change.start + change.len]
    }
}

/// One line diff with the default context for both the display diff and the patch.
pub fn generate_edit_diff(path: &str, old: &str, new: &str) -> EditDiff {
    let lines = LineDiff::new(old, new);
    let (diff, first_changed_line) = display_diff(&lines, DEFAULT_CONTEXT_LINES);
    EditDiff { diff, first_changed_line, patch: unified_patch(&lines, path, DEFAULT_CONTEXT_LINES) }
}

/// Display diff with line numbers and `context` lines around each change. Returns the diff and
/// the first changed line number in the new file.
pub fn generate_diff_string(old: &str, new: &str, context: usize) -> (String, Option<usize>) {
    display_diff(&LineDiff::new(old, new), context)
}

/// Standard unified patch with file headers and `context` lines around each change.
pub fn generate_unified_patch(path: &str, old: &str, new: &str, context: usize) -> String {
    unified_patch(&LineDiff::new(old, new), path, context)
}

/// `text.split('\n').count()` of the text that gave the tokens.
fn line_count(tokens: &[&str]) -> usize {
    tokens.len() + usize::from(tokens.last().is_none_or(|line| line.ends_with('\n')))
}

fn display_diff(lines: &LineDiff, context: usize) -> (String, Option<usize>) {
    let width = line_count(&lines.old).max(line_count(&lines.new)).to_string().len();
    let mut out = String::new();
    let (mut old_num, mut new_num) = (1usize, 1usize);
    let mut last_was_change = false;
    let mut first_changed = None;
    let skip = |out: &mut String| {
        let _ = writeln!(out, " {:width$} ...", "");
    };

    for (i, change) in lines.changes.iter().enumerate() {
        let text = lines.lines(change);
        if change.kind != ChangeKind::Equal {
            first_changed.get_or_insert(new_num);
            for line in text {
                let line = line.strip_suffix('\n').unwrap_or(line);
                if change.kind == ChangeKind::Added {
                    let _ = writeln!(out, "+{new_num:>width$} {line}");
                    new_num += 1;
                } else {
                    let _ = writeln!(out, "-{old_num:>width$} {line}");
                    old_num += 1;
                }
            }
            last_was_change = true;
            continue;
        }
        let next_is_change = lines.changes.get(i + 1).is_some_and(|next| next.kind != ChangeKind::Equal);
        let n = text.len();
        let shown: &[(usize, usize)] = match (last_was_change, next_is_change) {
            (true, true) if n <= context.saturating_mul(2) => &[(0, n)],
            (true, true) => &[(0, context), (n - context, n)],
            (true, false) => &[(0, context.min(n))],
            (false, true) => &[(n.saturating_sub(context), n)],
            (false, false) => &[],
        };
        let mut cursor = 0;
        for &(start, end) in shown {
            if start > cursor {
                skip(&mut out);
                old_num += start - cursor;
                new_num += start - cursor;
            }
            for line in &text[start..end] {
                let line = line.strip_suffix('\n').unwrap_or(line);
                let _ = writeln!(out, " {old_num:>width$} {line}");
                old_num += 1;
                new_num += 1;
            }
            cursor = end;
        }
        if cursor < n {
            if last_was_change || next_is_change {
                skip(&mut out);
            }
            old_num += n - cursor;
            new_num += n - cursor;
        }
        last_was_change = false;
    }
    out.pop();
    (out, first_changed)
}

/// jsdiff `structuredPatch` and `formatPatch`. The loop runs once past the last change, as jsdiff
/// does with its empty sentinel, to close the last hunk.
fn unified_patch(lines: &LineDiff, path: &str, context: usize) -> String {
    let changes = &lines.changes;
    let mut out = format!("--- {path}\n+++ {path}\n");
    let mut hunk: Vec<(char, &str)> = Vec::new();
    let (mut old_range_start, mut new_range_start) = (0, 0);
    let (mut old_line, mut new_line) = (1, 1);
    for i in 0..=changes.len() {
        let (kind, text) =
            changes.get(i).map_or((ChangeKind::Equal, &[][..]), |change| (change.kind, lines.lines(change)));
        if kind != ChangeKind::Equal {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if let Some(prev) = i.checked_sub(1).map(|prev| lines.lines(&changes[prev])) {
                    hunk = prev[prev.len().saturating_sub(context)..].iter().map(|line| (' ', *line)).collect();
                    old_range_start -= hunk.len();
                    new_range_start -= hunk.len();
                }
            }
            let sign = if kind == ChangeKind::Added { '+' } else { '-' };
            hunk.extend(text.iter().map(|line| (sign, *line)));
            if kind == ChangeKind::Added {
                new_line += text.len();
            } else {
                old_line += text.len();
            }
            continue;
        }
        if old_range_start != 0 {
            if text.len() <= context.saturating_mul(2) && i + 1 < changes.len() {
                hunk.extend(text.iter().map(|line| (' ', *line)));
            } else {
                let shown = text.len().min(context);
                hunk.extend(text[..shown].iter().map(|line| (' ', *line)));
                let old_lines = old_line - old_range_start + shown;
                let new_lines = new_line - new_range_start + shown;
                // An empty range starts at the line before it.
                let old_start = if old_lines == 0 { old_range_start - 1 } else { old_range_start };
                let new_start = if new_lines == 0 { new_range_start - 1 } else { new_range_start };
                let _ = writeln!(out, "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@");
                for (sign, line) in hunk.drain(..) {
                    let _ = match line.strip_suffix('\n') {
                        Some(line) => writeln!(out, "{sign}{line}"),
                        None => writeln!(out, "{sign}{line}\n\\ No newline at end of file"),
                    };
                }
                old_range_start = 0;
                new_range_start = 0;
            }
        }
        old_line += text.len();
        new_line += text.len();
    }
    out
}
