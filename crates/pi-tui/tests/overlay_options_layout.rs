//! Port of upstream `test/overlay-options.test.ts`: "width overflow protection",
//! "width percentage", "anchor positioning", and "margin".

mod overlay_common;

use overlay_common::*;
use pi_tui::{Component, OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue};

fn width(columns: i32) -> Option<SizeValue> {
    Some(SizeValue::Absolute(columns))
}

/// Shows `overlay` over empty content before start, like upstream, then renders.
fn render_overlay(h: &Harness, overlay: pi_tui::ComponentRef, options: OverlayOptions) {
    h.show(overlay, options);
    h.start();
    h.flush();
}

/// Bold red full-width rows.
struct StyledContent;

impl Component for StyledContent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let styled_line = format!("\x1b[1m\x1b[38;2;255;0;0m{}\x1b[0m", "X".repeat(width));
        vec![styled_line.clone(), styled_line.clone(), styled_line]
    }
}

/// Rows with an OSC 8 file link, like file paths in agent output.
struct HyperlinkContent;

impl Component for HyperlinkContent {
    fn render(&mut self, width: usize) -> Vec<String> {
        let link = "\x1b]8;;file:///path/to/file.ts\x07file.ts\x1b]8;;\x07";
        let line = format!("See {link} for details {}", "X".repeat(width - 30));
        vec![line.clone(), line.clone(), line]
    }
}

#[test]
fn should_truncate_overlay_lines_that_exceed_declared_width() {
    let h = Harness::with_editor(80, 24, None);
    // The overlay declares width 20 but renders much wider lines.
    let overlay = static_overlay(&[&"X".repeat(100)]);
    render_overlay(&h, overlay, OverlayOptions { width: width(20), ..Default::default() });
    // The render must not fail, and every row must exist.
    let viewport = h.viewport();
    assert_eq!(viewport.len(), 24);
    h.stop();
}

#[test]
fn should_handle_overlay_with_complex_ansi_sequences_without_crashing() {
    let h = Harness::with_editor(80, 24, None);
    let complex_line = format!(
        "{}{}{}",
        "\x1b[48;2;40;50;40m \x1b[38;2;128;128;128mSome styled content\x1b[39m\x1b[49m",
        "\x1b]8;;http://example.com\x07link\x1b]8;;\x07",
        " more content ".repeat(10)
    );
    let overlay = static_overlay(&[&complex_line, &complex_line, &complex_line]);
    render_overlay(&h, overlay, OverlayOptions { width: width(60), ..Default::default() });
    assert!(!h.viewport().is_empty());
    h.stop();
}

#[test]
fn should_handle_overlay_composited_on_styled_base_content() {
    let h = Harness::new(80, 24);
    h.tui.add_child(pi_tui::component(StyledContent));
    let overlay = static_overlay(&["OVERLAY"]);
    let options = OverlayOptions { width: width(20), anchor: Some(OverlayAnchor::Center), ..Default::default() };
    render_overlay(&h, overlay, options);
    let has_overlay = h.viewport().iter().any(|line| line.contains("OVERLAY"));
    assert!(has_overlay, "Overlay should be visible");
    h.stop();
}

#[test]
fn should_handle_wide_characters_at_overlay_boundary() {
    let h = Harness::with_editor(80, 24, None);
    // Each wide character takes 2 columns; the odd width may split one at the boundary.
    let overlay = static_overlay(&["中文日本語한글テスト漢字"]);
    render_overlay(&h, overlay, OverlayOptions { width: width(15), ..Default::default() });
    assert!(!h.viewport().is_empty());
    h.stop();
}

#[test]
fn should_handle_overlay_positioned_at_terminal_edge() {
    let h = Harness::with_editor(80, 24, None);
    // Column 60 with width 20 fits exactly at the right edge; the content is wider.
    let overlay = static_overlay(&[&"X".repeat(50)]);
    let options = OverlayOptions { col: Some(SizeValue::Absolute(60)), width: width(20), ..Default::default() };
    render_overlay(&h, overlay, options);
    assert!(!h.viewport().is_empty());
    h.stop();
}

#[test]
fn should_handle_overlay_on_base_content_with_osc_sequences() {
    let h = Harness::new(80, 24);
    h.tui.add_child(pi_tui::component(HyperlinkContent));
    let overlay = static_overlay(&["OVERLAY-TEXT"]);
    let options = OverlayOptions { anchor: Some(OverlayAnchor::Center), width: width(20), ..Default::default() };
    render_overlay(&h, overlay, options);
    assert!(!h.viewport().is_empty());
    h.stop();
}

#[test]
fn should_render_overlay_at_percentage_of_terminal_width() {
    let h = Harness::with_editor(100, 24, None);
    let overlay = static_overlay(&["test"]);
    render_overlay(&h, overlay.clone(), OverlayOptions { width: Some(SizeValue::Percent(50.0)), ..Default::default() });
    assert_eq!(overlay.borrow().requested_width, Some(50));
    h.stop();
}

#[test]
fn should_respect_min_width_when_width_percent_results_in_smaller_width() {
    let h = Harness::with_editor(100, 24, None);
    let overlay = static_overlay(&["test"]);
    let options = OverlayOptions { width: Some(SizeValue::Percent(10.0)), min_width: Some(30), ..Default::default() };
    render_overlay(&h, overlay.clone(), options);
    assert_eq!(overlay.borrow().requested_width, Some(30));
    h.stop();
}

#[test]
fn should_position_overlay_at_top_left() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["TOP-LEFT"]);
    let options = OverlayOptions { anchor: Some(OverlayAnchor::TopLeft), width: width(10), ..Default::default() };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    assert!(viewport[0].starts_with("TOP-LEFT"), "Expected TOP-LEFT at start, got: {}", viewport[0]);
    h.stop();
}

#[test]
fn should_position_overlay_at_bottom_right() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["BTM-RIGHT"]);
    let options = OverlayOptions { anchor: Some(OverlayAnchor::BottomRight), width: width(10), ..Default::default() };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    // Last row, ending at the last column.
    let last_row = &viewport[23];
    assert!(last_row.contains("BTM-RIGHT"), "Expected BTM-RIGHT on last row, got: {last_row}");
    assert!(last_row.trim_end().ends_with("BTM-RIGHT"), "Expected BTM-RIGHT at end, got: {last_row}");
    h.stop();
}

#[test]
fn should_position_overlay_at_top_center() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["CENTERED"]);
    let options = OverlayOptions { anchor: Some(OverlayAnchor::TopCenter), width: width(10), ..Default::default() };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    let first_row = &viewport[0];
    assert!(first_row.contains("CENTERED"), "Expected CENTERED on first row, got: {first_row}");
    // Width 10 in 80 columns centers at column 35.
    let col_index = first_row.find("CENTERED").map_or(-1, |i| i as i64);
    assert!((30..=40).contains(&col_index), "Expected centered, got col {col_index}");
    h.stop();
}

#[test]
fn should_clamp_negative_margins_to_zero() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["NEG-MARGIN"]);
    let options = OverlayOptions {
        anchor: Some(OverlayAnchor::TopLeft),
        width: width(12),
        margin: Some(OverlayMargin { top: -5, left: -10, right: 0, bottom: 0 }),
        ..Default::default()
    };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    // Negative margins clamp to 0, so the overlay sits at row 0, column 0.
    assert!(viewport[0].starts_with("NEG-MARGIN"), "Expected NEG-MARGIN at start of row 0, got: {}", viewport[0]);
    h.stop();
}

#[test]
fn should_respect_margin_as_number() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["MARGIN"]);
    let options = OverlayOptions {
        anchor: Some(OverlayAnchor::TopLeft),
        width: width(10),
        margin: Some(OverlayMargin::all(5)),
        ..Default::default()
    };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    assert!(!viewport[0].contains("MARGIN"), "Should not be on row 0");
    assert!(!viewport[4].contains("MARGIN"), "Should not be on row 4");
    assert!(viewport[5].contains("MARGIN"), "Expected MARGIN on row 5, got: {}", viewport[5]);
    let col_index = viewport[5].find("MARGIN").map_or(-1, |i| i as i64);
    assert_eq!(col_index, 5, "Expected col 5, got {col_index}");
    h.stop();
}

#[test]
fn should_respect_margin_object() {
    let h = Harness::with_editor(80, 24, None);
    let overlay = static_overlay(&["MARGIN"]);
    let options = OverlayOptions {
        anchor: Some(OverlayAnchor::TopLeft),
        width: width(10),
        margin: Some(OverlayMargin { top: 2, left: 3, right: 0, bottom: 0 }),
        ..Default::default()
    };
    render_overlay(&h, overlay, options);
    let viewport = h.viewport();
    assert!(viewport[2].contains("MARGIN"), "Expected MARGIN on row 2, got: {}", viewport[2]);
    let col_index = viewport[2].find("MARGIN").map_or(-1, |i| i as i64);
    assert_eq!(col_index, 3, "Expected col 3, got {col_index}");
    h.stop();
}
