//! Port of upstream `test/overlay-options.test.ts`: "offset", "percentage positioning",
//! "maxHeight", "absolute positioning", "stacked overlays", and "hiding after stop".

mod overlay_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use overlay_common::*;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{OverlayAnchor, OverlayOptions, SizeValue, Terminal, TerminalSink, Tui, TuiOptions};

fn abs(value: i32) -> Option<SizeValue> {
    Some(SizeValue::Absolute(value))
}

fn pct(value: f64) -> Option<SizeValue> {
    Some(SizeValue::Percent(value))
}

/// Shows `overlay` over empty content before start, like upstream, then renders.
fn render_overlay(h: &Harness, lines: &[&str], options: OverlayOptions) {
    h.show(static_overlay(lines), options);
    h.start();
    h.flush();
}

fn top_left(width: i32) -> OverlayOptions {
    OverlayOptions { anchor: Some(OverlayAnchor::TopLeft), width: abs(width), ..Default::default() }
}

#[test]
fn should_apply_offset_x_and_offset_y_from_anchor_position() {
    let h = Harness::with_editor(80, 24, None);
    render_overlay(&h, &["OFFSET"], OverlayOptions { offset_x: Some(10), offset_y: Some(5), ..top_left(10) });
    let viewport = h.viewport();
    assert!(viewport[5].contains("OFFSET"), "Expected OFFSET on row 5, got: {}", viewport[5]);
    let col_index = viewport[5].find("OFFSET").map_or(-1, |i| i as i64);
    assert_eq!(col_index, 10, "Expected col 10, got {col_index}");
    h.stop();
}

#[test]
fn should_position_with_row_percent_and_col_percent() {
    let h = Harness::with_editor(80, 24, None);
    // 50% centers both ways.
    render_overlay(
        &h,
        &["PCT"],
        OverlayOptions { width: abs(10), row: pct(50.0), col: pct(50.0), ..Default::default() },
    );
    let found_row = h.viewport().iter().position(|line| line.contains("PCT")).map_or(-1, |i| i as i64);
    // Roughly centered vertically: row 11 or 12 of 24.
    assert!((10..=13).contains(&found_row), "Expected centered row, got {found_row}");
    h.stop();
}

#[test]
fn row_percent_0_should_position_at_top() {
    let h = Harness::with_editor(80, 24, None);
    render_overlay(&h, &["TOP"], OverlayOptions { width: abs(10), row: pct(0.0), ..Default::default() });
    let viewport = h.viewport();
    assert!(viewport[0].contains("TOP"), "Expected TOP on row 0, got: {}", viewport[0]);
    h.stop();
}

#[test]
fn row_percent_100_should_position_at_bottom() {
    let h = Harness::with_editor(80, 24, None);
    render_overlay(&h, &["BOTTOM"], OverlayOptions { width: abs(10), row: pct(100.0), ..Default::default() });
    let viewport = h.viewport();
    assert!(viewport[23].contains("BOTTOM"), "Expected BOTTOM on last row, got: {}", viewport[23]);
    h.stop();
}

#[test]
fn should_truncate_overlay_to_max_height() {
    let h = Harness::with_editor(80, 24, None);
    let lines = ["Line 1", "Line 2", "Line 3", "Line 4", "Line 5"];
    render_overlay(&h, &lines, OverlayOptions { max_height: abs(3), ..Default::default() });
    let content = h.viewport().join("\n");
    assert!(content.contains("Line 1"), "Should include Line 1");
    assert!(content.contains("Line 2"), "Should include Line 2");
    assert!(content.contains("Line 3"), "Should include Line 3");
    assert!(!content.contains("Line 4"), "Should NOT include Line 4");
    assert!(!content.contains("Line 5"), "Should NOT include Line 5");
    h.stop();
}

#[test]
fn should_truncate_overlay_to_max_height_percent() {
    let h = Harness::with_editor(80, 10, None);
    // 10 lines in a 10 row terminal with a 50% max height show 5 lines.
    let lines = ["L1", "L2", "L3", "L4", "L5", "L6", "L7", "L8", "L9", "L10"];
    render_overlay(&h, &lines, OverlayOptions { max_height: pct(50.0), ..Default::default() });
    let content = h.viewport().join("\n");
    assert!(content.contains("L1"), "Should include L1");
    assert!(content.contains("L5"), "Should include L5");
    assert!(!content.contains("L6"), "Should NOT include L6");
    h.stop();
}

#[test]
fn row_and_col_should_override_anchor() {
    let h = Harness::with_editor(80, 24, None);
    // Row and column win over the bottom-right anchor.
    let options = OverlayOptions {
        anchor: Some(OverlayAnchor::BottomRight),
        row: abs(3),
        col: abs(5),
        width: abs(10),
        ..Default::default()
    };
    render_overlay(&h, &["ABSOLUTE"], options);
    let viewport = h.viewport();
    assert!(viewport[3].contains("ABSOLUTE"), "Expected ABSOLUTE on row 3, got: {}", viewport[3]);
    let col_index = viewport[3].find("ABSOLUTE").map_or(-1, |i| i as i64);
    assert_eq!(col_index, 5, "Expected col 5, got {col_index}");
    h.stop();
}

#[test]
fn should_render_multiple_overlays_with_later_ones_on_top() {
    let h = Harness::with_editor(80, 24, None);
    h.show(static_overlay(&["FIRST-OVERLAY"]), top_left(20));
    // The second overlay covers part of the first.
    h.show(static_overlay(&["SECOND"]), top_left(10));
    h.start();
    h.flush();
    let viewport = h.viewport();
    assert!(viewport[0].contains("SECOND"), "Expected SECOND on row 0, got: {}", viewport[0]);
    h.stop();
}

#[test]
fn should_handle_overlays_at_different_positions_without_interference() {
    let h = Harness::with_editor(80, 24, None);
    h.show(static_overlay(&["TOP-LEFT"]), top_left(15));
    let bottom_right =
        OverlayOptions { anchor: Some(OverlayAnchor::BottomRight), width: abs(15), ..Default::default() };
    h.show(static_overlay(&["BTM-RIGHT"]), bottom_right);
    h.start();
    h.flush();
    let viewport = h.viewport();
    assert!(viewport[0].contains("TOP-LEFT"), "Expected TOP-LEFT on row 0, got: {}", viewport[0]);
    assert!(viewport[23].contains("BTM-RIGHT"), "Expected BTM-RIGHT on row 23, got: {}", viewport[23]);
    h.stop();
}

#[test]
fn should_properly_hide_overlays_in_stack_order() {
    let h = Harness::with_editor(80, 24, None);
    h.show(static_overlay(&["FIRST"]), top_left(10));
    h.show(static_overlay(&["SECOND"]), top_left(10));
    h.start();
    h.flush();
    assert!(h.viewport()[0].contains("SECOND"), "SECOND should be visible initially");

    h.tui.hide_overlay();
    h.flush();
    assert!(h.viewport()[0].contains("FIRST"), "FIRST should be visible after hiding SECOND");
    h.stop();
}

/// Upstream `CursorTrackingTerminal`: records the last hide or show cursor call.
struct CursorTrackingTerminal {
    inner: VirtualTerminal,
    cursor_visible: bool,
}

impl Terminal for CursorTrackingTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.inner.start(sink);
    }

    fn stop(&mut self) {
        self.inner.stop();
    }

    fn drain_input(&mut self, max: Duration, idle: Duration) {
        self.inner.drain_input(max, idle);
    }

    fn write(&mut self, data: &str) {
        self.inner.write(data);
    }

    fn columns(&self) -> usize {
        self.inner.columns()
    }

    fn rows(&self) -> usize {
        self.inner.rows()
    }

    fn kitty_protocol_active(&self) -> bool {
        self.inner.kitty_protocol_active()
    }

    fn hide_cursor(&mut self) {
        self.cursor_visible = false;
        self.inner.hide_cursor();
    }

    fn show_cursor(&mut self) {
        self.cursor_visible = true;
        self.inner.show_cursor();
    }
}

fn cursor_tracking_tui() -> (Rc<RefCell<CursorTrackingTerminal>>, Tui) {
    let terminal =
        Rc::new(RefCell::new(CursorTrackingTerminal { inner: VirtualTerminal::new(80, 24), cursor_visible: true }));
    let env = Rc::new(pi_tui::env::MapEnv::new([("TERM", "xterm-256color")]));
    let tui = Tui::new_main_screen(terminal.clone(), TuiOptions { env: Some(env), ..Default::default() });
    (terminal, tui)
}

#[test]
fn hide_overlay_leaves_the_cursor_visible() {
    let (terminal, tui) = cursor_tracking_tui();
    tui.start();
    tui.show_overlay(static_overlay(&["OVERLAY"]), non_capturing());

    tui.stop(Default::default());
    tui.hide_overlay();

    assert!(terminal.borrow().cursor_visible);
}

#[test]
fn overlay_handle_hide_leaves_the_cursor_visible() {
    let (terminal, tui) = cursor_tracking_tui();
    tui.start();
    let handle = tui.show_overlay(static_overlay(&["OVERLAY"]), non_capturing());

    tui.stop(Default::default());
    handle.hide();

    assert!(terminal.borrow().cursor_visible);
}
