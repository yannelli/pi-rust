//! Shared truncation utilities for tool outputs.
//!
//! Two independent limits apply (lines and bytes); whichever is hit first wins.

use serde::Serialize;

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
/// Max chars per grep match line.
pub const GREP_MAX_LINE_LENGTH: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// Field names match the TypeScript `TruncationResult`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct TruncationOptions {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for TruncationOptions {
    fn default() -> Self {
        Self { max_lines: DEFAULT_MAX_LINES, max_bytes: DEFAULT_MAX_BYTES }
    }
}

impl TruncationOptions {
    /// Byte limit only, for outputs whose row count is already capped.
    pub fn bytes_only() -> Self {
        Self { max_lines: usize::MAX, ..Self::default() }
    }
}

pub(crate) fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as a human-readable size.
pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn untruncated(content: &str, total_lines: usize, opts: TruncationOptions) -> TruncationResult {
    TruncationResult {
        content: content.to_string(),
        truncated: false,
        truncated_by: None,
        total_lines,
        total_bytes: content.len(),
        output_lines: total_lines,
        output_bytes: content.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines: opts.max_lines,
        max_bytes: opts.max_bytes,
    }
}

/// Keep the first lines/bytes. Never returns partial lines; if the first line alone exceeds
/// the byte limit, returns empty content with `first_line_exceeds_limit` set.
pub fn truncate_head(content: &str, opts: TruncationOptions) -> TruncationResult {
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= opts.max_lines && content.len() <= opts.max_bytes {
        return untruncated(content, total_lines, opts);
    }
    let base = TruncationResult {
        truncated: true,
        truncated_by: Some(TruncatedBy::Bytes),
        output_lines: 0,
        output_bytes: 0,
        content: String::new(),
        ..untruncated(content, total_lines, opts)
    };
    if lines[0].len() > opts.max_bytes {
        return TruncationResult { first_line_exceeds_limit: true, ..base };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    for (i, line) in lines.iter().enumerate().take(opts.max_lines) {
        let line_bytes = line.len() + usize::from(i > 0);
        if bytes + line_bytes > opts.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    if kept.len() >= opts.max_lines && bytes <= opts.max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    let out = kept.join("\n");
    TruncationResult {
        truncated_by: Some(truncated_by),
        output_lines: kept.len(),
        output_bytes: out.len(),
        content: out,
        ..base
    }
}

/// Keep the last lines/bytes. May return a partial line when the last line alone exceeds
/// the byte limit (`last_line_partial`).
pub fn truncate_tail(content: &str, opts: TruncationOptions) -> TruncationResult {
    let lines = split_lines_for_counting(content);
    let total_lines = lines.len();
    if total_lines <= opts.max_lines && content.len() <= opts.max_bytes {
        return untruncated(content, total_lines, opts);
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated_by = TruncatedBy::Lines;
    let mut last_line_partial = false;
    for line in lines.iter().rev() {
        if kept.len() >= opts.max_lines {
            break;
        }
        let line_bytes = line.len() + usize::from(!kept.is_empty());
        if bytes + line_bytes > opts.max_bytes {
            truncated_by = TruncatedBy::Bytes;
            if kept.is_empty() {
                let partial = truncate_str_to_bytes_from_end(line, opts.max_bytes);
                bytes = partial.len();
                kept.push(partial);
                last_line_partial = true;
            }
            break;
        }
        kept.push(line);
        bytes += line_bytes;
    }
    if kept.len() >= opts.max_lines && bytes <= opts.max_bytes {
        truncated_by = TruncatedBy::Lines;
    }
    kept.reverse();
    let out = kept.join("\n");
    TruncationResult {
        truncated: true,
        truncated_by: Some(truncated_by),
        output_lines: kept.len(),
        output_bytes: out.len(),
        content: out,
        last_line_partial,
        ..untruncated(content, total_lines, opts)
    }
}

/// Last `max_bytes` bytes of `s`, starting at a character boundary.
pub(crate) fn truncate_str_to_bytes_from_end(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Truncate a single line to `max_chars` characters, adding a `... [truncated]` suffix.
pub fn truncate_line(line: &str, max_chars: usize) -> (String, bool) {
    match line.char_indices().nth(max_chars) {
        None => (line.to_string(), false),
        Some((idx, _)) => (format!("{}... [truncated]", &line[..idx]), true),
    }
}
