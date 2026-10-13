//! Shared truncation utilities for tool outputs. Two independent limits apply, and whichever is hit first wins:
//! a line limit (default 2000 lines) and a byte limit (default 50 KiB). Partial lines are never returned.

use serde::{Deserialize, Serialize};

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// Which limit truncated the content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TruncatedBy {
    Lines,
    Bytes,
}

/// The outcome of a truncation, with the sizes of the original and the kept content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationResult {
    pub content: String,
    pub truncated: bool,
    /// `None` when not truncated.
    pub truncated_by: Option<TruncatedBy>,
    pub total_lines: usize,
    pub total_bytes: usize,
    /// Complete lines in the truncated output.
    pub output_lines: usize,
    pub output_bytes: usize,
    /// Whether the last line was partially truncated (only for the tail truncation edge case).
    pub last_line_partial: bool,
    /// Whether the first line exceeded the byte limit (for head truncation).
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

/// Limits of a truncation; absent fields take the defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TruncationOptions {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
}

/// Line and byte totals of a text known only by a prefix; see `truncate_head_of`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextTotals {
    /// Lines counted like `truncate_head`, ignoring a trailing newline.
    pub lines: usize,
    pub bytes: usize,
}

/// UTF-8 byte length of `content`.
pub fn utf8_byte_length(content: &str) -> usize {
    content.len()
}

fn split_lines_for_counting(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// Format bytes as a human-readable size: `1023B`, `1.5KB`, `3.0MB`.
pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", fixed_one(bytes, 1024))
    } else {
        format!("{}MB", fixed_one(bytes, 1024 * 1024))
    }
}

/// `(value / unit).toFixed(1)`: exact, with ties rounded up like JavaScript.
fn fixed_one(value: u64, unit: u64) -> String {
    let tenths = u128::from(value) * 10;
    let unit = u128::from(unit);
    let mut rounded = tenths / unit;
    if (tenths % unit) * 2 >= unit {
        rounded += 1;
    }
    format!("{}.{}", rounded / 10, rounded % 10)
}

/// Truncate content from the head (keep the first lines and bytes), for file reads that show the beginning.
/// If the first line alone exceeds the byte limit, the content is empty and `first_line_exceeds_limit` is set.
pub fn truncate_head(content: &str, options: TruncationOptions) -> TruncationResult {
    let totals = TextTotals { lines: split_lines_for_counting(content).len(), bytes: content.len() };
    truncate_head_of(content, totals, options)
}

/// `truncate_head` of a text known by a prefix and its totals. The prefix must be the whole text, longer than
/// `max_bytes + 1` bytes, or hold at least `max_lines` newlines; then the result equals `truncate_head` of the text.
pub fn truncate_head_of(prefix: &str, totals: TextTotals, options: TruncationOptions) -> TruncationResult {
    let max_lines = options.max_lines.unwrap_or(DEFAULT_MAX_LINES);
    let max_bytes = options.max_bytes.unwrap_or(DEFAULT_MAX_BYTES);
    let TextTotals { lines: total_lines, bytes: total_bytes } = totals;
    let result =
        |content: String, truncated_by: Option<TruncatedBy>, output_lines, output_bytes, first_line_exceeds| {
            TruncationResult {
                content,
                truncated: truncated_by.is_some(),
                truncated_by,
                total_lines,
                total_bytes,
                output_lines,
                output_bytes,
                last_line_partial: false,
                first_line_exceeds_limit: first_line_exceeds,
                max_lines,
                max_bytes,
            }
        };

    if total_lines <= max_lines && total_bytes <= max_bytes {
        return result(prefix.to_string(), None, total_lines, total_bytes, false);
    }

    let lines = split_lines_for_counting(prefix);
    if lines.first().map_or(0, |line| line.len()) > max_bytes {
        return result(String::new(), Some(TruncatedBy::Bytes), 0, 0, true);
    }

    let mut output_count = 0;
    let mut output_bytes = 0;
    let mut truncated_by = None;
    for (index, line) in lines.iter().enumerate().take(max_lines) {
        let line_bytes = line.len() + usize::from(index > 0);
        if output_bytes + line_bytes > max_bytes {
            truncated_by = Some(TruncatedBy::Bytes);
            break;
        }
        output_count += 1;
        output_bytes += line_bytes;
    }
    // Without a byte break, only omitted lines prove the line limit was reached; otherwise a trailing newline
    // exceeded the bytes.
    let truncated_by =
        truncated_by.unwrap_or(if output_count < total_lines { TruncatedBy::Lines } else { TruncatedBy::Bytes });
    let content = lines[..output_count].join("\n");
    let bytes = content.len();
    result(content, Some(truncated_by), output_count, bytes, false)
}
