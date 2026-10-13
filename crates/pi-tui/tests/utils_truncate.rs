//! Ported from upstream test/truncate-to-width.test.ts (truncateToWidth cases).

use pi_tui::utils::{truncate_to_width, truncate_to_width_with, visible_width};

#[test]
fn keeps_output_within_width_for_very_large_unicode_input() {
    let text = "🙂界".repeat(100_000);
    let truncated = truncate_to_width_with(&text, 40, "…", false);
    assert!(visible_width(&truncated) <= 40);
    assert!(truncated.ends_with("…\x1b[0m"));
}

#[test]
fn preserves_ansi_styling_for_kept_text_and_resets_before_and_after_ellipsis() {
    let text = format!("\x1b[31m{}\x1b[0m", "hello ".repeat(1000));
    let truncated = truncate_to_width_with(&text, 20, "…", false);
    assert!(visible_width(&truncated) <= 20);
    assert!(truncated.contains("\x1b[31m"));
    assert!(truncated.ends_with("\x1b[0m…\x1b[0m"));
}

#[test]
fn closes_a_bel_terminated_osc8_link_when_truncating_its_label() {
    let open = "\x1b]8;;https://example.com\x07";
    let close = "\x1b]8;;\x07";
    let text = format!("{open}some-longer-label-here{close}");
    assert_eq!(truncate_to_width(&text, 15), format!("{open}some-longer-{close}\x1b[0m...\x1b[0m"));
}

#[test]
fn handles_malformed_ansi_escape_prefixes_without_hanging() {
    let text = format!("abc\x1bnot-ansi {}", "🙂".repeat(1000));
    let truncated = truncate_to_width_with(&text, 20, "…", false);
    assert!(visible_width(&truncated) <= 20);
}

#[test]
fn clips_wide_ellipsis_safely_and_brackets_it_with_resets() {
    assert_eq!(truncate_to_width_with("abcdef", 1, "🙂", false), "");
    assert_eq!(truncate_to_width_with("abcdef", 2, "🙂", false), "\x1b[0m🙂\x1b[0m");
    assert!(visible_width(&truncate_to_width_with("abcdef", 2, "🙂", false)) <= 2);
}

#[test]
fn returns_the_original_text_when_it_already_fits_even_if_ellipsis_is_too_wide() {
    assert_eq!(truncate_to_width_with("a", 2, "🙂", false), "a");
    assert_eq!(truncate_to_width_with("界", 2, "🙂", false), "界");
}

#[test]
fn pads_truncated_output_to_requested_width() {
    let truncated = truncate_to_width_with("🙂界🙂界🙂界", 8, "…", true);
    assert_eq!(visible_width(&truncated), 8);
}

#[test]
fn adds_a_trailing_reset_when_truncating_without_an_ellipsis() {
    let truncated = truncate_to_width_with(&format!("\x1b[31m{}", "hello".repeat(100)), 10, "", false);
    assert!(visible_width(&truncated) <= 10);
    assert!(truncated.ends_with("\x1b[0m"));
}

#[test]
fn keeps_a_contiguous_prefix_instead_of_skipping_a_wide_grapheme_and_resuming_later() {
    let truncated = truncate_to_width_with("🙂\t界 \x1b_abc\x07", 7, "…", true);
    assert_eq!(truncated, "🙂\t\x1b[0m…\x1b[0m ");
}

#[test]
fn handles_zero_width_empty_input_and_ascii_fast_paths() {
    assert_eq!(truncate_to_width("hello", 0), "");
    assert_eq!(truncate_to_width_with("", 3, "...", true), "   ");
    assert_eq!(truncate_to_width("hello", 5), "hello");
    assert_eq!(truncate_to_width("hello world", 8), "hello\x1b[0m...\x1b[0m");
    assert_eq!(truncate_to_width_with("hi", 4, "...", true), "hi  ");
}
