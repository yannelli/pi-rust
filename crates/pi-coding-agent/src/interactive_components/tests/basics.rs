use std::time::Duration;

use pi_tui::Component;

use crate::interactive_components::test_support::{Calls, plain, setup};
use crate::interactive_components::*;

#[test]
fn dynamic_border_spans_the_width() {
    let mut border = DynamicBorder::new();
    assert_eq!(plain(&border.render(5)), ["─────"]);
    assert_eq!(plain(&border.render(0)), ["─"]);
    let mut custom = DynamicBorder::with_color(|text| format!("<{text}>"));
    assert_eq!(custom.render(2), ["<──>"]);
}

#[test]
fn key_hints_format_bound_keys() {
    let _kb = setup();
    assert_eq!(key_text("tui.select.cancel"), "escape/ctrl+c");
    assert_eq!(key_display_text("app.models.save"), "Ctrl+S");
    assert_eq!(key_text("app.session.new"), "");
    assert_eq!(format_key_text("ctrl+x/shift+y"), "ctrl+x/shift+y");
    assert_eq!(format_key_display_text("ctrl+x/shift+y"), "Ctrl+X/Shift+Y");
    let hint = pi_tui::utils::strip_terminal_sequences(&key_hint("tui.select.confirm", "select"));
    assert_eq!(hint, "enter select");
    let raw = pi_tui::utils::strip_terminal_sequences(&raw_key_hint("↑↓", "navigate"));
    assert_eq!(raw, "↑↓ navigate");
}

#[test]
fn countdown_ticks_each_second_and_expires() {
    let ticks = Calls::new();
    let expired = Calls::new();
    let _timer = CountdownTimer::new(Duration::from_millis(1500), None, ticks.recorder(), expired.counter());
    assert_eq!(ticks.get(), [2]);
    pi_tui::ui_loop::run_for(Duration::from_millis(2300)).unwrap();
    assert_eq!(ticks.get(), [2, 1, 0]);
    assert_eq!(expired.len(), 1);
}

#[test]
fn disposed_countdown_stops() {
    let ticks = Calls::new();
    let expired = Calls::new();
    let timer = CountdownTimer::new(Duration::from_secs(3), None, ticks.recorder(), expired.counter());
    timer.disposer()();
    pi_tui::ui_loop::run_for(Duration::from_millis(1200)).unwrap();
    assert_eq!(ticks.get(), [3]);
    assert!(expired.is_empty());
}
