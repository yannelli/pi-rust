//! Port of upstream `test/env-truncate.test.ts`.

use pi_durable::truncate::{TruncatedBy, TruncationOptions, format_size, truncate_head, utf8_byte_length};

fn limits(max_bytes: usize, max_lines: usize) -> TruncationOptions {
    TruncationOptions { max_bytes: Some(max_bytes), max_lines: Some(max_lines) }
}

#[test]
fn reports_utf8_byte_counts_in_truncation_results() {
    let content = "aé🙂\nb";
    let result = truncate_head(content, limits(100, 10));
    assert!(!result.truncated);
    assert_eq!((result.total_bytes, result.output_bytes), (9, 9));
}

/// Upstream runs a fallback counter in a runtime without `Buffer`; Rust strings are UTF-8, so the length is the count.
/// Lone surrogates have no Rust string form and are left out.
#[test]
fn counts_utf8_bytes_and_truncates_correctly() {
    let inputs = ["", "ascii", "é", "中", "🙂", "a🙂b", "\u{07ff}\u{0800}\u{ffff}"];
    let lengths: Vec<usize> = inputs.iter().map(|input| utf8_byte_length(input)).collect();
    assert_eq!(lengths, [0, 5, 2, 3, 4, 6, 8]);
    let head = truncate_head("aé🙂\nb", limits(7, 10));
    assert_eq!((head.content.as_str(), head.output_bytes, head.truncated_by), ("aé🙂", 7, Some(TruncatedBy::Bytes)));
}

#[test]
fn does_not_count_a_trailing_newline_as_an_extra_line() {
    let head = truncate_head("line\nline\nline\n", limits(100, 3));
    assert_eq!((head.truncated, head.total_lines, head.output_lines), (false, 3, 3));
}

#[test]
fn truncates_head_by_line_limits() {
    let head = truncate_head("one\ntwo\nthree\nfour", limits(100, 2));
    assert_eq!(head.content, "one\ntwo");
    assert!(head.truncated);
    assert_eq!((head.truncated_by, head.total_lines, head.output_lines), (Some(TruncatedBy::Lines), 4, 2));
}

#[test]
fn reports_bytes_when_only_a_trailing_newline_exceeds_limits_at_the_line_cap() {
    let head = truncate_head("hello\nworld\n", limits(11, 2));
    assert_eq!(head.content, "hello\nworld");
    assert!(head.truncated);
    assert_eq!((head.truncated_by, head.total_lines, head.output_lines), (Some(TruncatedBy::Bytes), 2, 2));
}

#[test]
fn truncates_head_on_utf8_byte_limits_without_partial_lines() {
    let result = truncate_head("éé\nabc", limits(4, 10));
    assert_eq!(result.content, "éé");
    assert!(result.truncated);
    assert_eq!((result.truncated_by, result.output_bytes), (Some(TruncatedBy::Bytes), 4));
    assert!(!result.first_line_exceeds_limit);
}

#[test]
fn reports_head_truncation_when_the_first_line_exceeds_the_byte_limit() {
    let result = truncate_head("éé\nabc", limits(3, 10));
    assert_eq!(result.content, "");
    assert!(result.truncated);
    assert_eq!(result.truncated_by, Some(TruncatedBy::Bytes));
    assert!(result.first_line_exceeds_limit);
}

#[test]
fn formats_sizes() {
    assert_eq!(format_size(1023), "1023B");
    assert_eq!(format_size(1536), "1.5KB");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    // JavaScript's toFixed rounds this exact tie up.
    assert_eq!(format_size(1280), "1.3KB");
}

#[test]
fn serializes_like_upstream() {
    let json = serde_json::to_value(truncate_head("a", TruncationOptions::default())).unwrap();
    assert_eq!(json["truncatedBy"], serde_json::Value::Null);
    assert_eq!(json["firstLineExceedsLimit"], false);
}
