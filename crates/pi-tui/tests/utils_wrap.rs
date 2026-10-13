//! Ported from upstream test/wrap-ansi.test.ts.

use pi_tui::utils::{visible_width, wrap_text_with_ansi};
use regex::Regex;

const UNDERLINE_ON: &str = "\x1b[4m";
const UNDERLINE_OFF: &str = "\x1b[24m";
const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[31m";

#[test]
fn should_not_apply_underline_style_before_the_styled_text() {
    let url = "https://example.com/very/long/path/that/will/wrap";
    let text = format!("read this thread {UNDERLINE_ON}{url}{UNDERLINE_OFF}");
    let wrapped = wrap_text_with_ansi(&text, 40);
    assert_eq!(wrapped[0], "read this thread");
    assert!(wrapped[1].starts_with(UNDERLINE_ON));
    assert!(wrapped[1].contains("https://"));
}

#[test]
fn should_not_have_whitespace_before_underline_reset_code() {
    let text = format!("{UNDERLINE_ON}underlined text here {UNDERLINE_OFF}more");
    let wrapped = wrap_text_with_ansi(&text, 18);
    assert!(!wrapped[0].contains(&format!(" {UNDERLINE_OFF}")));
}

#[test]
fn should_not_bleed_underline_to_padding() {
    let url = "https://example.com/very/long/path/that/will/definitely/wrap";
    let text = format!("prefix {UNDERLINE_ON}{url}{UNDERLINE_OFF} suffix");
    let wrapped = wrap_text_with_ansi(&text, 30);
    for line in &wrapped[1..wrapped.len() - 1] {
        if line.contains(UNDERLINE_ON) {
            assert!(line.ends_with(UNDERLINE_OFF));
            assert!(!line.ends_with(RESET));
        }
    }
}

#[test]
fn should_preserve_background_color_across_wrapped_lines_without_full_reset() {
    let bg_blue = "\x1b[44m";
    let text = format!("{bg_blue}hello world this is blue background text{RESET}");
    let wrapped = wrap_text_with_ansi(&text, 15);
    for line in &wrapped {
        assert!(line.contains(bg_blue));
    }
    for line in &wrapped[..wrapped.len() - 1] {
        assert!(!line.ends_with(RESET));
    }
}

#[test]
fn should_reset_underline_but_preserve_background_when_wrapping_underlined_text_inside_background() {
    let text = format!("\x1b[41mprefix {UNDERLINE_ON}UNDERLINED_CONTENT_THAT_WRAPS{UNDERLINE_OFF} suffix{RESET}");
    let wrapped = wrap_text_with_ansi(&text, 20);
    for line in &wrapped {
        assert!(line.contains("[41m") || line.contains(";41m") || line.contains("[41;"));
    }
    for line in &wrapped[..wrapped.len() - 1] {
        let has_underline = line.contains("[4m") || line.contains("[4;") || line.contains(";4m");
        if has_underline && !line.contains(UNDERLINE_OFF) {
            assert!(line.ends_with(UNDERLINE_OFF));
            assert!(!line.ends_with(RESET));
        }
    }
}

#[test]
fn should_handle_lf_crlf_and_cr_line_endings() {
    assert_eq!(wrap_text_with_ansi("first\nsecond\r\nthird\rfourth", 80), ["first", "second", "third", "fourth"]);
}

#[test]
fn should_preserve_ansi_state_across_crlf_and_cr_line_endings() {
    assert_eq!(
        wrap_text_with_ansi(&format!("{RED}first\r\nsecond\rthird{RESET}"), 80),
        [format!("{RED}first"), format!("{RED}second"), format!("{RED}third{RESET}")]
    );
}

#[test]
fn should_wrap_plain_text_correctly() {
    let wrapped = wrap_text_with_ansi("hello world this is a test", 10);
    assert!(wrapped.len() > 1);
    for line in &wrapped {
        assert!(visible_width(line) <= 10);
    }
}

#[test]
fn should_break_cjk_runs_at_grapheme_boundaries_after_latin_text() {
    let text = "This is an example 中文汉字测试段落内容中文汉字测试段落内容.";
    let wrapped = wrap_text_with_ansi(text, 40);
    assert_eq!(wrapped, ["This is an example 中文汉字测试段落内容", "中文汉字测试段落内容."]);
    for line in &wrapped {
        assert!(visible_width(line) <= 40);
    }
}

#[test]
fn should_preserve_color_codes_when_wrapping_cjk_runs() {
    let text = format!("{RED}This is an example 中文汉字测试段落内容中文汉字测试段落内容.{RESET}");
    let wrapped = wrap_text_with_ansi(&text, 40);
    assert_eq!(wrapped.len(), 2);
    assert_eq!(wrapped[0], format!("{RED}This is an example 中文汉字测试段落内容"));
    assert_eq!(wrapped[1], format!("{RED}中文汉字测试段落内容.{RESET}"));
    for line in &wrapped {
        assert!(visible_width(line) <= 40);
    }
}

#[test]
fn should_ignore_osc133_semantic_markers_in_visible_width() {
    assert_eq!(visible_width("\x1b]133;A\x07hello\x1b]133;B\x07"), 5);
}

#[test]
fn should_ignore_osc_sequences_terminated_with_st_in_visible_width() {
    assert_eq!(visible_width("\x1b]133;A\x1b\\hello\x1b]133;B\x1b\\"), 5);
}

#[test]
fn should_treat_isolated_regional_indicators_as_width_2() {
    assert_eq!(visible_width("🇨"), 2);
    assert_eq!(visible_width("🇨🇳"), 2);
}

#[test]
fn should_truncate_trailing_whitespace_that_exceeds_width() {
    let wrapped = wrap_text_with_ansi("  ", 1);
    assert!(visible_width(&wrapped[0]) <= 1);
}

#[test]
fn should_preserve_color_codes_across_wraps() {
    let wrapped = wrap_text_with_ansi(&format!("{RED}hello world this is red{RESET}"), 10);
    for line in &wrapped[1..] {
        assert!(line.starts_with(RED));
    }
    for line in &wrapped[..wrapped.len() - 1] {
        assert!(!line.ends_with(RESET));
    }
}

const URL: &str = "https://example.com";

#[test]
fn re_emits_osc8_open_at_the_start_of_continuation_lines() {
    let input = format!("\x1b]8;;{URL}\x1b\\0123456789\x1b]8;;\x1b\\");
    let lines = wrap_text_with_ansi(&input, 6);
    let osc = Regex::new("\x1b\\]8;;[^\x1b\x07]*\x1b\\\\").unwrap();
    let sgr = Regex::new("\x1b\\[[0-9;]*m").unwrap();
    let open = format!("\x1b]8;;{URL}\x1b\\");
    for line in &lines {
        let stripped = sgr.replace_all(&osc.replace_all(line, ""), "").to_string();
        if !stripped.trim().is_empty() {
            assert!(line.starts_with(&open) || line.contains(&open), "{line:?} has visible text but no OSC 8 re-open");
        }
    }
}

#[test]
fn closes_osc8_before_each_line_break() {
    let input = format!("\x1b]8;;{URL}\x1b\\0123456789\x1b]8;;\x1b\\");
    let lines = wrap_text_with_ansi(&input, 6);
    for line in &lines[..lines.len() - 1] {
        if line.contains(&format!("\x1b]8;;{URL}\x1b\\")) {
            assert!(line.ends_with("\x1b]8;;\x1b\\"), "non-final line {line:?} does not close the link");
        }
    }
}

#[test]
fn preserves_bel_terminators_when_wrapping_oauth_style_hyperlinks() {
    let url = format!("https://example.com/oauth/{}", "a".repeat(32));
    let input = format!("\x1b]8;;{url}\x07{url}\x1b]8;;\x07");
    let lines = wrap_text_with_ansi(&input, 20);
    assert!(lines.len() > 1);
    for line in &lines {
        assert!(line.contains(&format!("\x1b]8;;{url}\x07")), "{line:?} does not reopen the link with BEL");
        assert!(!line.contains(&format!("\x1b]8;;{url}\x1b\\")), "{line:?} reopens the link with ST");
    }
    for line in &lines[..lines.len() - 1] {
        assert!(line.ends_with("\x1b]8;;\x07"), "{line:?} does not close the link with BEL");
    }
}

#[test]
fn does_not_emit_osc8_sequences_on_lines_that_are_outside_the_hyperlink() {
    let input = format!("before \x1b]8;;{URL}\x1b\\link\x1b]8;;\x1b\\ after");
    let lines = wrap_text_with_ansi(&input, 80);
    assert_eq!(lines.len(), 1);
    let open_count = Regex::new("\x1b\\]8;;https:[^\x1b]+\x1b\\\\").unwrap().find_iter(&lines[0]).count();
    let close_count = lines[0].matches("\x1b]8;;\x1b\\").count();
    assert_eq!(open_count, 1);
    assert_eq!(close_count, 1);
}

#[test]
fn returns_one_empty_line_for_empty_input() {
    assert_eq!(wrap_text_with_ansi("", 10), [""]);
    assert_eq!(wrap_text_with_ansi("a\n\nb", 10), ["a", "", "b"]);
}
