//! Port of upstream `test/tui-overlay-style-leak.test.ts`, `test/overlay-short-content.test.ts`,
//! `test/regression-overlay-cjk-boundary.test.ts`, and the TUI case of `test/tab-width.test.ts`.

mod render_common;

use pi_tui::testing::VirtualTerminal;
use pi_tui::utils::extract_segments;
use pi_tui::{
    Component, OverlayOptions, SizeValue, Tui, TuiStopOptions, composite_tui_line, slice_by_column, ui_loop,
    visible_width,
};
use render_common::{LoggingTerminal, cell_italic, erased, main_tui, strings, test_component, xterm_viewport};

fn at(row: i32, col: i32, width: i32) -> OverlayOptions {
    OverlayOptions {
        row: Some(SizeValue::Absolute(row)),
        col: Some(SizeValue::Absolute(col)),
        width: Some(SizeValue::Absolute(width)),
        ..OverlayOptions::default()
    }
}

/// Upstream `renderAndFlush`: a forced render, then the render pipeline settles.
fn render_and_flush(tui: &Tui) {
    tui.request_render(true);
    ui_loop::run_for(std::time::Duration::from_millis(20)).expect("render");
}

#[test]
fn should_not_leak_styles_when_a_trailing_reset_sits_beyond_the_last_visible_column_no_overlay() {
    let width = 20;
    let base_line = format!("\x1b[3m{}\x1b[23m", "X".repeat(width));

    let terminal = VirtualTerminal::shared(width, 6);
    let tui = main_tui(&terminal);
    tui.add_child(erased(&test_component(&[&base_line, "INPUT"])));
    tui.start();
    render_and_flush(&tui);
    assert!(!cell_italic(&terminal.borrow(), 1, 0));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn should_not_leak_styles_when_overlay_slicing_drops_trailing_sgr_resets() {
    let width = 20;
    let base_line = format!("\x1b[3m{}\x1b[23m", "X".repeat(width));

    let terminal = VirtualTerminal::shared(width, 6);
    let tui = main_tui(&terminal);
    tui.add_child(erased(&test_component(&[&base_line, "INPUT"])));

    tui.show_overlay(erased(&test_component(&["OVR"])), at(0, 5, 3));
    tui.start();
    render_and_flush(&tui);

    assert!(!cell_italic(&terminal.borrow(), 1, 0));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn should_render_overlay_when_content_is_shorter_than_terminal_height() {
    // 24 terminal rows but only 3 content lines.
    let terminal = VirtualTerminal::shared(80, 24);
    let tui = main_tui(&terminal);
    tui.add_child(erased(&test_component(&["Line 1", "Line 2", "Line 3"])));

    // Centered overlay, around row 10 of 24.
    let overlay = test_component(&["OVERLAY_TOP", "OVERLAY_MID", "OVERLAY_BOT"]);
    tui.show_overlay(erased(&overlay), OverlayOptions::default());

    tui.start();
    render_common::wait_for_render();

    let viewport = terminal.borrow().viewport();
    let has_overlay = viewport.iter().any(|line| line.contains("OVERLAY"));
    assert!(has_overlay, "Overlay should be visible when content is shorter than terminal: {viewport:#?}");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn excludes_a_wide_grapheme_from_before_when_overlay_starts_inside_it() {
    let segments = extract_segments("abcd让EFGH", 5, 9, 11, true);

    assert_eq!(segments.before, "abcd");
    assert_eq!(segments.before_width, 4);
    assert_eq!(visible_width(&segments.before), segments.before_width);
    assert_eq!(segments.after, "H");
    assert_eq!(segments.after_width, 1);
}

#[test]
fn keeps_ascii_before_segment_behavior_at_the_same_boundary() {
    let segments = extract_segments("abcdG EFGH", 5, 9, 11, true);

    assert_eq!(segments.before, "abcdG");
    assert_eq!(segments.before_width, 5);
    assert_eq!(visible_width(&segments.before), segments.before_width);
}

#[test]
fn composites_an_overlay_at_the_requested_column_when_it_starts_inside_a_wide_grapheme() {
    let out = composite_tui_line("abcd让EFGH", "│XX│", 5, 4, 20);
    let prefix = slice_by_column(&out, 0, 5, true);
    let overlay = slice_by_column(&out, 5, 4, true);

    assert!(!out.contains('让'));
    assert_eq!(visible_width(&out), 20);
    assert_eq!(visible_width(&prefix), 5);
    assert_eq!(visible_width(&overlay), 4);
    assert!(overlay.contains("│XX│"));
}

#[test]
fn composites_an_overlay_when_it_starts_at_a_wide_grapheme_boundary() {
    let out = composite_tui_line("abcd让EFGH", "│XX│", 4, 4, 20);
    let overlay = slice_by_column(&out, 4, 4, true);

    assert!(!out.contains('让'));
    assert_eq!(visible_width(&out), 20);
    assert_eq!(visible_width(&overlay), 4);
    assert!(overlay.contains("│XX│"));
}

/// Upstream `FullViewportContent`: three base lines padded to the full width.
struct FullViewportContent;

impl Component for FullViewportContent {
    fn render(&mut self, width: usize) -> Vec<String> {
        ["base 0", "base 1", "base 2"].iter().map(|line| format!("{line:<width$}")).collect()
    }
}

#[test]
fn keeps_tab_containing_overlays_on_one_physical_terminal_row() {
    let terminal = LoggingTerminal::shared(16, 3);
    let tui = main_tui(&terminal);
    tui.add_child(erased(&pi_tui::component(FullViewportContent)));
    tui.show_overlay(erased(&test_component(&["\tX"])), at(1, 4, 4));
    tui.start();

    render_common::wait_for_render();
    assert_eq!(
        xterm_viewport(&terminal.borrow().vt),
        strings(&["base 0          ", "base   X        ", "base 2          "])
    );
    assert!(!terminal.borrow().get_writes().contains('\t'));
    tui.stop(TuiStopOptions::default());
}
