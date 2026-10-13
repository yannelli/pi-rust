//! Ported from upstream test/regression-slice-by-column-ansi-order.test.ts, plus slicing and cell-range cases.

use pi_tui::utils::{
    GraphemeCellRange, extract_segments, get_grapheme_cell_range, get_osc8_link_at_column, slice_by_column,
    slice_with_width,
};

// https://github.com/earendil-works/pi/issues/10169
#[test]
fn keeps_a_reset_at_the_slice_start_after_earlier_style_codes() {
    let line = "\x1b[32mfoo\x1b[39m bar";
    assert_eq!(slice_by_column(line, 3, 4, true), "\x1b[32m\x1b[39m bar");
}

#[test]
fn does_not_leak_color_into_text_after_a_highlighted_token() {
    let line = "Another \x1b[35malpha\x1b[39m line with \x1b[35mbeta\x1b[39m later.";
    let after = slice_by_column(line, 13, 100, true);
    assert_eq!(after, "\x1b[35m\x1b[39m line with \x1b[35mbeta\x1b[39m later.");
}

#[test]
fn handles_wide_characters_at_slice_boundaries() {
    assert_eq!(slice_by_column("a界b", 0, 2, true), "a");
    assert_eq!(slice_by_column("a界b", 0, 2, false), "a界");
    let sliced = slice_with_width("a界b", 1, 3, true);
    assert_eq!((sliced.text.as_str(), sliced.width), ("界b", 3));
    assert_eq!(slice_by_column("abc", 1, 0, false), "");
}

#[test]
fn extracts_segments_with_inherited_styling() {
    let segments = extract_segments("\x1b[31mabcdef\x1b[0mgh", 2, 4, 3, false);
    assert_eq!(segments.before, "\x1b[31mab");
    assert_eq!(segments.before_width, 2);
    assert_eq!(segments.after, "\x1b[31mef\x1b[0mg");
    assert_eq!(segments.after_width, 3);
}

#[test]
fn finds_grapheme_cells_and_hyperlinks_by_column() {
    let line = "a\x1b[1m界\x1b[0mb";
    assert_eq!(get_grapheme_cell_range(line, 2), Some(GraphemeCellRange { start: 1, end: 3 }));
    assert_eq!(get_grapheme_cell_range(line, 3), Some(GraphemeCellRange { start: 3, end: 4 }));
    assert_eq!(get_grapheme_cell_range(line, 4), None);

    let linked = "x\x1b]8;;https://a.test\x1b\\link\x1b]8;;\x1b\\";
    assert_eq!(get_osc8_link_at_column(linked, 0), None);
    assert_eq!(get_osc8_link_at_column(linked, 3).as_deref(), Some("https://a.test"));
    assert_eq!(get_osc8_link_at_column(linked, 5), None);
}
