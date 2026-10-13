//! Port of the "TUI fake cursor" cases of upstream `test/tui-render.test.ts`.

mod render_common;

use pi_tui::{
    CURSOR_MARKER, Input, OverlayOptions, SizeValue, Tui, TuiOptions, TuiStopOptions, component, render_fake_cursor,
    slice_by_column,
};
use render_common::{LoggingTerminal, erased, main_tui, options, test_component, wait_for_render};

fn options_with_hardware_cursor(show: bool) -> TuiOptions {
    TuiOptions { show_hardware_cursor: Some(show), ..options() }
}

#[test]
fn draws_the_focused_fake_cursor_in_reverse_video_and_omits_it_when_the_hardware_cursor_is_shown() {
    let terminal = LoggingTerminal::shared(20, 5);
    let tui = main_tui(&terminal);
    let input = component(Input::default());
    input.borrow_mut().set_value("abc");
    tui.add_child(erased(&input));
    tui.set_focus(Some(erased(&input)));
    tui.start();
    wait_for_render();
    assert!(terminal.borrow().get_writes().contains("> \x1b[7ma\x1b[27mbc"));

    terminal.borrow_mut().clear_writes();
    tui.set_show_hardware_cursor(true);
    wait_for_render();
    let writes = terminal.borrow().get_writes();
    assert!(!writes.contains("\x1b[7m"));
    assert!(!writes.contains("\x1b_pi:"));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn keeps_the_fake_cursor_of_an_unfocused_input_when_the_hardware_cursor_is_shown() {
    let terminal = LoggingTerminal::shared(20, 5);
    let tui = Tui::new_main_screen(terminal.clone(), options_with_hardware_cursor(true));
    let input = component(Input::default());
    input.borrow_mut().set_value("abc");
    tui.add_child(erased(&input));
    tui.start();
    wait_for_render();
    assert!(terminal.borrow().get_writes().contains("> \x1b[7ma\x1b[27mbc"));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn omits_the_focused_fake_cursor_when_truncation_dropped_its_end_marker() {
    let terminal = LoggingTerminal::shared(20, 5);
    let tui = Tui::new_main_screen(terminal.clone(), options_with_hardware_cursor(true));
    let line = slice_by_column(&format!("ab{CURSOR_MARKER}{}def", render_fake_cursor("c")), 0, 3, true);
    let base = test_component(&[&line]);
    tui.add_child(erased(&base));
    tui.start();
    wait_for_render();
    let writes = terminal.borrow().get_writes();
    assert!(!writes.contains("\x1b[7m"));
    assert!(writes.ends_with("\x1b[3G"), "{writes:?}");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn keeps_the_cursor_when_an_overlay_ends_directly_left_of_it() {
    for show_hardware_cursor in [false, true] {
        let terminal = LoggingTerminal::shared(20, 5);
        let tui = Tui::new_main_screen(terminal.clone(), options_with_hardware_cursor(show_hardware_cursor));
        let base = test_component(&[&format!("abc{CURSOR_MARKER}{}ef", render_fake_cursor("d"))]);
        tui.add_child(erased(&base));
        let overlay = test_component(&["XXX"]);
        let placement = OverlayOptions {
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(0)),
            width: Some(SizeValue::Absolute(3)),
            ..OverlayOptions::default()
        };
        tui.show_overlay(erased(&overlay), placement);
        tui.start();
        wait_for_render();

        let writes = terminal.borrow().get_writes();
        assert!(writes.contains("XXX"));
        assert_eq!(writes.contains("\x1b[7md"), !show_hardware_cursor);
        // The hardware cursor is positioned at column 4 in both modes.
        assert!(writes.ends_with("\x1b[4G"), "{writes:?}");
        tui.stop(TuiStopOptions::default());
    }
}
