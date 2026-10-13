//! Ported from upstream test/tab-width.test.ts. The overlay case needs the TUI renderer and is not here.

use pi_tui::utils::{extract_segments, normalize_terminal_output, slice_with_width, visible_width};

const TEXT: &str = "out 192M\t.pi/skill-tests/results-ha";

#[test]
fn keeps_slice_helper_widths_consistent_with_visible_width() {
    let slice = slice_with_width(TEXT, 0, 10, true);
    assert_eq!(slice.text, "out 192M");
    assert_eq!(slice.width, 8);
    assert_eq!(visible_width(&slice.text), slice.width);
}

#[test]
fn keeps_overlay_segment_widths_consistent_with_visible_width() {
    let segments = extract_segments(TEXT, 10, 13, 10, true);
    assert_eq!(segments.before, "out 192M");
    assert_eq!(segments.before_width, 8);
    assert_eq!(visible_width(&segments.before), segments.before_width);

    let tab_fits = extract_segments(TEXT, 11, 13, 10, true);
    assert_eq!(tab_fits.before, "out 192M\t");
    assert_eq!(tab_fits.before_width, 11);
    assert_eq!(visible_width(&tab_fits.before), tab_fits.before_width);
}

#[test]
fn keeps_tabs_inside_terminal_control_sequences_byte_identical() {
    for control in ["\x1b]8;;https://example.test/a\tb\x07", "\x1b]0;window\ttitle\x1b\\", "\x1b_payload\tdata\x1b\\"] {
        assert_eq!(normalize_terminal_output(&format!("{control}label\ttext")), format!("{control}label   text"));
    }
}
