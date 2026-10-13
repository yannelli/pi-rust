//! Port of the "TUI resize handling" and "TUI content shrinkage" cases of upstream
//! `test/tui-render.test.ts`, and of upstream `test/tui-shrink.test.ts`.

mod render_common;

use pi_tui::testing::VirtualTerminal;
use pi_tui::{Tui, TuiStopOptions};
use render_common::{
    LoggingTerminal, erased, main_tui, numbered, options_with_env, strings, test_component, wait_for_render,
};

#[test]
fn triggers_full_re_render_when_terminal_height_changes() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = Tui::new_main_screen(terminal.clone(), options_with_env(&[]));
    let component = test_component(&["Line 0", "Line 1", "Line 2"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let initial_redraws = tui.full_redraws();

    terminal.borrow_mut().resize(40, 15);
    wait_for_render();

    assert!(tui.full_redraws() > initial_redraws, "Height change should trigger full redraw");
    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Line 0"), "Content preserved after height change");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn skips_full_re_render_on_height_changes_in_termux() {
    let terminal = LoggingTerminal::shared(40, 10);
    let tui = Tui::new_main_screen(terminal.clone(), options_with_env(&[("TERMUX_VERSION", "1")]));
    let component = test_component(&[]);
    tui.add_child(erased(&component));
    component.borrow_mut().lines = numbered("Line ", 20);
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    let initial_redraws = tui.full_redraws();
    for height in [15, 8, 14, 11] {
        terminal.borrow_mut().resize(40, height);
        wait_for_render();
    }

    let writes = terminal.borrow().get_writes();
    assert_eq!(tui.full_redraws(), initial_redraws, "Height change should not trigger full redraw");
    assert!(!writes.contains("\x1b[2J"), "Height change should not clear the screen");
    assert!(!writes.contains("\x1b[3J"), "Height change should not clear scrollback");

    let viewport = terminal.borrow().vt.viewport();
    assert!(viewport.join("\n").contains("Line 19"), "Latest content remains visible after resize");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn triggers_full_re_render_when_terminal_width_changes() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let initial_redraws = tui.full_redraws();

    terminal.borrow_mut().resize(60, 10);
    wait_for_render();

    assert!(tui.full_redraws() > initial_redraws, "Width change should trigger full redraw");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn clears_empty_rows_when_content_shrinks_significantly() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    tui.set_clear_on_shrink(true);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4", "Line 5"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let initial_redraws = tui.full_redraws();

    component.borrow_mut().lines = strings(&["Line 0", "Line 1"]);
    tui.request_render(false);
    wait_for_render();

    assert!(tui.full_redraws() > initial_redraws, "Content shrinkage should trigger full redraw");
    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Line 0"), "First line preserved");
    assert!(viewport[1].contains("Line 1"), "Second line preserved");
    assert_eq!(viewport[2].trim(), "", "Line 2 should be cleared");
    assert_eq!(viewport[3].trim(), "", "Line 3 should be cleared");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn handles_shrink_to_single_line() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    tui.set_clear_on_shrink(true);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    component.borrow_mut().lines = strings(&["Only line"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Only line"), "Single line rendered");
    assert_eq!(viewport[1].trim(), "", "Line 1 should be cleared");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn handles_shrink_to_empty() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    tui.set_clear_on_shrink(true);
    let component = test_component(&["Line 0", "Line 1", "Line 2"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    component.borrow_mut().lines = Vec::new();
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert_eq!(viewport[0].trim(), "", "Line 0 should be cleared");
    assert_eq!(viewport[1].trim(), "", "Line 1 should be cleared");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn clears_all_rendered_lines_when_content_shrinks_to_zero() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let content = test_component(&["first", "second", "third"]);
    tui.add_child(erased(&content));
    tui.start();
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport.iter().any(|line| line.contains("first")));
    assert!(viewport.iter().any(|line| line.contains("second")));
    assert!(viewport.iter().any(|line| line.contains("third")));

    tui.clear();
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(!viewport.iter().any(|line| line.contains("first")), "first line should be cleared");
    assert!(!viewport.iter().any(|line| line.contains("second")), "second line should be cleared");
    assert!(!viewport.iter().any(|line| line.contains("third")), "third line should be cleared");
    tui.stop(TuiStopOptions::default());
}
