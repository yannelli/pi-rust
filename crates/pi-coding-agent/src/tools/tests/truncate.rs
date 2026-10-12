use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::truncate::*;

fn opts(max_lines: usize, max_bytes: usize) -> TruncationOptions {
    TruncationOptions { max_lines, max_bytes }
}

#[test]
fn head_keeps_first_lines() {
    let r = truncate_head("a\nb\nc\nd", opts(2, 100));
    assert!(r.truncated);
    assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
    assert_eq!(r.content, "a\nb");
    assert_eq!((r.total_lines, r.output_lines), (4, 2));
}

#[test]
fn head_byte_limit_and_first_line_exceeds() {
    let r = truncate_head("aaaa\nbbbb\ncccc", opts(100, 9));
    assert_eq!(r.content, "aaaa\nbbbb");
    assert_eq!(r.truncated_by, Some(TruncatedBy::Bytes));
    let r = truncate_head("aaaaaaaaaa\nb", opts(100, 5));
    assert!(r.first_line_exceeds_limit);
    assert_eq!(r.content, "");
}

#[test]
fn head_untouched_when_within_limits() {
    let r = truncate_head("a\nb\n", TruncationOptions::default());
    assert!(!r.truncated);
    assert_eq!(r.total_lines, 2);
    assert_eq!(r.content, "a\nb\n");
}

#[test]
fn tail_keeps_last_lines_and_partial_line() {
    let r = truncate_tail("a\nb\nc\nd", opts(2, 100));
    assert_eq!(r.content, "c\nd");
    assert_eq!(r.truncated_by, Some(TruncatedBy::Lines));
    let r = truncate_tail("short\nééééé", opts(100, 5));
    assert!(r.last_line_partial);
    assert_eq!(r.content, "éé");
}

#[test]
fn format_size_and_truncate_line() {
    assert_eq!(format_size(512), "512B");
    assert_eq!(format_size(2048), "2.0KB");
    assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    assert_eq!(truncate_line("abcdef", 3), ("abc... [truncated]".to_string(), true));
    assert_eq!(truncate_line("abc", 3), ("abc".to_string(), false));
}

#[test]
fn truncation_serializes_with_upstream_field_names() {
    let v = serde_json::to_value(truncate_head("a\nb", opts(1, 100))).unwrap();
    assert_eq!(v["truncatedBy"], "lines");
    assert_eq!(v["firstLineExceedsLimit"], false);
    assert_eq!(v["outputLines"], 1);
}

#[test]
fn resolves_relative_tilde_and_at_paths() {
    let cwd = std::path::Path::new("/work/project");
    assert_eq!(resolve_to_cwd("src/../a.rs", cwd), std::path::PathBuf::from("/work/project/a.rs"));
    assert_eq!(resolve_to_cwd("@./b.rs", cwd), std::path::PathBuf::from("/work/project/b.rs"));
    assert_eq!(resolve_to_cwd("/abs/c.rs", cwd), std::path::PathBuf::from("/abs/c.rs"));
    if let Some(home) = std::env::var_os("HOME") {
        assert_eq!(resolve_to_cwd("~/x", cwd), std::path::Path::new(&home).join("x"));
    }
}
