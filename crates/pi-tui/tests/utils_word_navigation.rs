//! Ported from upstream test/word-navigation.test.ts. Cursors are byte offsets.

use pi_tui::utils::WordSegment;
use pi_tui::word_navigation::{WordNavigationOptions, find_word_backward, find_word_forward};

fn back(text: &str, cursor: usize) -> usize {
    find_word_backward(text, cursor, &WordNavigationOptions::default())
}

fn fwd(text: &str, cursor: usize) -> usize {
    find_word_forward(text, cursor, &WordNavigationOptions::default())
}

#[test]
fn backward_basic_words() {
    assert_eq!(back("hello world", 11), 6);
    assert_eq!(back("hello world", 6), 0);
}

#[test]
fn backward_dotted() {
    assert_eq!(back("foo.bar", 7), 4);
    assert_eq!(back("foo.bar", 4), 3);
    assert_eq!(back("foo.bar", 3), 0);
}

#[test]
fn backward_colon() {
    assert_eq!(back("foo:bar", 7), 4);
    assert_eq!(back("foo:bar", 4), 3);
    assert_eq!(back("foo:bar", 3), 0);
}

#[test]
fn backward_path() {
    let text = "path/to/file";
    assert_eq!(back(text, 12), 8);
    assert_eq!(back(text, 8), 7);
    assert_eq!(back(text, 7), 5);
    assert_eq!(back(text, 5), 4);
    assert_eq!(back(text, 4), 0);
}

#[test]
fn backward_cjk_mixed() {
    // Each ideograph is 3 bytes. ICU's dictionary groups "你好" and "世界" (upstream stops at 2 and 0);
    // without a dictionary, UAX #29 stops at every ideograph.
    let text = "你好世界 test";
    assert_eq!(back(text, text.len()), 13);
    assert_eq!(back(text, 13), 9);
    assert_eq!(back(text, 9), 6);
    assert_eq!(back(text, 6), 3);
    assert_eq!(back(text, 3), 0);
}

#[test]
fn backward_whitespace_at_boundaries() {
    assert_eq!(back("  hello  ", 9), 2);
    assert_eq!(back("  hello  ", 2), 0);
}

#[test]
fn backward_punctuation_run() {
    assert_eq!(back("foo...bar", 9), 6);
    assert_eq!(back("foo...bar", 6), 3);
    assert_eq!(back("foo...bar", 3), 0);
}

#[test]
fn backward_cursor_at_0_returns_0() {
    assert_eq!(back("hello", 0), 0);
}

#[test]
fn forward_basic_words() {
    assert_eq!(fwd("hello world", 0), 5);
    assert_eq!(fwd("hello world", 5), 11);
}

#[test]
fn forward_dotted() {
    assert_eq!(fwd("foo.bar", 0), 3);
    assert_eq!(fwd("foo.bar", 3), 4);
    assert_eq!(fwd("foo.bar", 4), 7);
}

#[test]
fn forward_colon() {
    assert_eq!(fwd("foo:bar", 0), 3);
    assert_eq!(fwd("foo:bar", 3), 4);
    assert_eq!(fwd("foo:bar", 4), 7);
}

#[test]
fn forward_path() {
    let text = "path/to/file";
    assert_eq!(fwd(text, 0), 4);
    assert_eq!(fwd(text, 4), 5);
    assert_eq!(fwd(text, 5), 7);
    assert_eq!(fwd(text, 7), 8);
    assert_eq!(fwd(text, 8), 12);
}

#[test]
fn forward_cjk_mixed() {
    let text = "你好世界 test";
    let first_end = fwd(text, 0);
    assert!(first_end > 0);
    assert!(first_end <= "你好世界".len());
    let mut pos = 0;
    while pos < text.len() {
        let next = fwd(text, pos);
        if next == pos {
            break;
        }
        pos = next;
    }
    assert_eq!(pos, text.len());
}

#[test]
fn forward_whitespace_at_boundaries() {
    assert_eq!(fwd("  hello  ", 0), 7);
    assert_eq!(fwd("  hello  ", 7), 9);
}

#[test]
fn forward_punctuation_run() {
    assert_eq!(fwd("foo...bar", 0), 3);
    assert_eq!(fwd("foo...bar", 3), 6);
    assert_eq!(fwd("foo...bar", 6), 9);
}

#[test]
fn forward_cursor_at_end_returns_end() {
    assert_eq!(fwd("hello", 5), 5);
}

const MARKER: &str = "[paste #1 +5 lines]";

/// Builds options whose segmenter returns pre-split segments for the slices the functions request.
fn atomic_options(text: &str) -> WordNavigationOptions<'static> {
    let full = text.to_string();
    WordNavigationOptions::default()
        .with_segment(move |input| {
            let spans: &[(usize, usize, bool)] = if input == full {
                &[(0, 5, true), (5, 6, false), (6, 25, true), (25, 26, false), (26, 31, true)]
            } else if input == &full[..26] {
                &[(0, 5, true), (5, 6, false), (6, 25, true), (25, 26, false)]
            } else if input == &full[6..] {
                &[(0, 19, true), (19, 20, false), (20, 25, true)]
            } else {
                &[]
            };
            spans
                .iter()
                .map(|&(start, end, is_word_like)| WordSegment {
                    segment: &input[start..end],
                    index: start,
                    is_word_like,
                })
                .collect()
        })
        .with_atomic_segment(|segment| segment == MARKER)
}

#[test]
fn backward_skips_word_then_stops_before_atomic_marker() {
    let text = format!("hello {MARKER} world");
    assert_eq!(find_word_backward(&text, text.len(), &atomic_options(&text)), 26);
}

#[test]
fn backward_skips_whitespace_then_atomic_marker_as_one_unit() {
    let text = format!("hello {MARKER} world");
    assert_eq!(find_word_backward(&text, 26, &atomic_options(&text)), 6);
}

#[test]
fn forward_skips_atomic_marker_as_one_unit() {
    let text = format!("hello {MARKER} world");
    assert_eq!(find_word_forward(&text, 6, &atomic_options(&text)), 6 + MARKER.len());
}

#[test]
fn clamps_cursors_inside_multibyte_characters() {
    assert_eq!(back("ab é", 4), 0);
    assert_eq!(fwd("é x", 1), 2);
}
