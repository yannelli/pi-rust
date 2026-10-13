//! Port of the "TUI differential rendering" cases of upstream `test/tui-render.test.ts`.

mod render_common;

use pi_tui::TuiStopOptions;
use pi_tui::testing::VirtualTerminal;
use render_common::{cell_italic, erased, main_tui, numbered, strings, test_component, wait_for_render};

#[test]
fn tracks_cursor_correctly_when_content_shrinks_with_unchanged_remaining_lines() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    // Shrink to 3 lines, all identical to before.
    component.borrow_mut().lines = strings(&["Line 0", "Line 1", "Line 2"]);
    tui.request_render(false);
    wait_for_render();

    // A change on line 1 lands there only when the cursor row was tracked correctly.
    component.borrow_mut().lines = strings(&["Line 0", "CHANGED", "Line 2"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[1].contains("CHANGED"), "Expected \"CHANGED\" on line 1, got: {}", viewport[1]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn renders_correctly_when_only_a_middle_line_changes_spinner_case() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Header", "Working...", "Footer"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    for frame in ["|", "/", "-", "\\"] {
        component.borrow_mut().lines = vec!["Header".to_string(), format!("Working {frame}"), "Footer".to_string()];
        tui.request_render(false);
        wait_for_render();

        let viewport = terminal.borrow().viewport();
        assert!(viewport[0].contains("Header"), "Header preserved: {}", viewport[0]);
        assert!(viewport[1].contains(&format!("Working {frame}")), "Spinner updated: {}", viewport[1]);
        assert!(viewport[2].contains("Footer"), "Footer preserved: {}", viewport[2]);
    }
    tui.stop(TuiStopOptions::default());
}

#[test]
fn resets_styles_after_each_rendered_line() {
    let terminal = VirtualTerminal::shared(20, 6);
    let tui = main_tui(&terminal);
    let component = test_component(&["\x1b[3mItalic", "Plain"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    assert!(!cell_italic(&terminal.borrow(), 1, 0));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn renders_correctly_when_first_line_changes_but_rest_stays_same() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    component.borrow_mut().lines = strings(&["CHANGED", "Line 1", "Line 2", "Line 3"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("CHANGED"), "First line changed: {}", viewport[0]);
    assert!(viewport[1].contains("Line 1"), "Line 1 preserved: {}", viewport[1]);
    assert!(viewport[2].contains("Line 2"), "Line 2 preserved: {}", viewport[2]);
    assert!(viewport[3].contains("Line 3"), "Line 3 preserved: {}", viewport[3]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn renders_correctly_when_last_line_changes_but_rest_stays_same() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    component.borrow_mut().lines = strings(&["Line 0", "Line 1", "Line 2", "CHANGED"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Line 0"), "Line 0 preserved: {}", viewport[0]);
    assert!(viewport[1].contains("Line 1"), "Line 1 preserved: {}", viewport[1]);
    assert!(viewport[2].contains("Line 2"), "Line 2 preserved: {}", viewport[2]);
    assert!(viewport[3].contains("CHANGED"), "Last line changed: {}", viewport[3]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn renders_correctly_when_multiple_non_adjacent_lines_change() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2", "Line 3", "Line 4"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    component.borrow_mut().lines = strings(&["Line 0", "CHANGED 1", "Line 2", "CHANGED 3", "Line 4"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Line 0"), "Line 0 preserved: {}", viewport[0]);
    assert!(viewport[1].contains("CHANGED 1"), "Line 1 changed: {}", viewport[1]);
    assert!(viewport[2].contains("Line 2"), "Line 2 preserved: {}", viewport[2]);
    assert!(viewport[3].contains("CHANGED 3"), "Line 3 changed: {}", viewport[3]);
    assert!(viewport[4].contains("Line 4"), "Line 4 preserved: {}", viewport[4]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn handles_transition_from_content_to_empty_and_back_to_content() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["Line 0", "Line 1", "Line 2"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("Line 0"), "Initial content rendered");

    component.borrow_mut().lines = Vec::new();
    tui.request_render(false);
    wait_for_render();

    // Content added after the empty state must render correctly.
    component.borrow_mut().lines = strings(&["New Line 0", "New Line 1"]);
    tui.request_render(false);
    wait_for_render();

    let viewport = terminal.borrow().viewport();
    assert!(viewport[0].contains("New Line 0"), "New content rendered: {}", viewport[0]);
    assert!(viewport[1].contains("New Line 1"), "New content line 1: {}", viewport[1]);
    tui.stop(TuiStopOptions::default());
}

#[test]
fn full_re_renders_when_deleted_lines_move_the_viewport_upward() {
    let terminal = VirtualTerminal::shared(20, 5);
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    component.borrow_mut().lines = numbered("Line ", 12);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let initial_redraws = tui.full_redraws();

    component.borrow_mut().lines = numbered("Line ", 7);
    tui.request_render(false);
    wait_for_render();

    assert!(tui.full_redraws() > initial_redraws, "Shrink should trigger a full redraw");
    assert_eq!(terminal.borrow().viewport(), strings(&["Line 2", "Line 3", "Line 4", "Line 5", "Line 6"]));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn appends_after_a_shrink_without_another_full_redraw_once_the_viewport_is_reset() {
    let terminal = VirtualTerminal::shared(20, 5);
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    component.borrow_mut().lines = numbered("Line ", 8);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();

    let initial_redraws = tui.full_redraws();

    component.borrow_mut().lines = strings(&["Line 0", "Line 1"]);
    tui.request_render(false);
    wait_for_render();

    assert!(tui.full_redraws() > initial_redraws, "Shrink should reset the viewport with a full redraw");
    let redraws_after_shrink = tui.full_redraws();

    component.borrow_mut().lines = strings(&["Line 0", "Line 1", "Line 2"]);
    tui.request_render(false);
    wait_for_render();

    assert_eq!(tui.full_redraws(), redraws_after_shrink, "Append should stay on the differential path");
    assert_eq!(terminal.borrow().viewport(), strings(&["Line 0", "Line 1", "Line 2", "", ""]));
    tui.stop(TuiStopOptions::default());
}

#[test]
fn clears_stale_content_when_max_lines_rendered_was_inflated_by_a_transient_component() {
    let terminal = VirtualTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let chat = test_component(&[]);
    let editor = test_component(&[]);
    tui.add_child(erased(&chat));
    tui.add_child(erased(&editor));

    let long_chat = numbered("Chat ", 15);
    let short_chat = numbered("Chat ", 12);
    let editor_lines = strings(&["Editor 0", "Editor 1", "Editor 2"]);
    let selector_lines = numbered("Selector ", 8);

    chat.borrow_mut().lines = long_chat;
    editor.borrow_mut().lines = editor_lines.clone();
    tui.start();
    wait_for_render();

    editor.borrow_mut().lines = selector_lines;
    tui.request_render(false);
    wait_for_render();

    editor.borrow_mut().lines = editor_lines;
    tui.request_render(false);
    wait_for_render();

    let redraws_before_switch = tui.full_redraws();
    chat.borrow_mut().lines = short_chat;
    tui.request_render(false);
    wait_for_render();

    assert!(tui.full_redraws() > redraws_before_switch, "Branch switch should trigger a full redraw");

    let viewport = terminal.borrow().viewport();
    for (i, line) in viewport.iter().enumerate().take(10) {
        assert!(!line.contains("Chat 12"), "Stale \"Chat 12\" at viewport row {i}");
        assert!(!line.contains("Chat 13"), "Stale \"Chat 13\" at viewport row {i}");
        assert!(!line.contains("Chat 14"), "Stale \"Chat 14\" at viewport row {i}");
    }

    assert_eq!(
        viewport,
        strings(&[
            "Chat 5", "Chat 6", "Chat 7", "Chat 8", "Chat 9", "Chat 10", "Chat 11", "Editor 0", "Editor 1", "Editor 2",
        ])
    );
    tui.stop(TuiStopOptions::default());
}
