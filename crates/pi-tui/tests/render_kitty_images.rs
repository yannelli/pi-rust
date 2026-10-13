//! Port of the "TUI Kitty image cleanup" cases of upstream `test/tui-render.test.ts`.
//! Every case holds the global state lock: images and cell sizes are process-wide.

mod render_common;
mod support;

use std::sync::MutexGuard;

use pi_tui::TuiStopOptions;
use pi_tui::env::global_state_lock;
use pi_tui::terminal_image::{
    CellDimensions, ImageDimensions, ImageProtocol, KittyEncodeOptions, TerminalCapabilities, delete_kitty_image,
    encode_kitty, reset_capabilities_cache, set_capabilities, set_cell_dimensions,
};
use pi_tui::{Component, Terminal};
use render_common::{LoggingTerminal, erased, main_tui, strings, test_component, wait_for_render};
use support::{Image, ImageOptions, ImageTheme};

/// Holds the global state lock and restores capabilities and cell dimensions on drop.
struct KittyGuard {
    _lock: MutexGuard<'static, ()>,
}

impl Drop for KittyGuard {
    fn drop(&mut self) {
        reset_capabilities_cache();
        set_cell_dimensions(CellDimensions { width_px: 9, height_px: 18 });
    }
}

fn kitty_terminal() -> KittyGuard {
    let lock = global_state_lock();
    set_capabilities(TerminalCapabilities { images: Some(ImageProtocol::Kitty), true_color: true, hyperlinks: true });
    set_cell_dimensions(CellDimensions { width_px: 10, height_px: 10 });
    KittyGuard { _lock: lock }
}

fn image_lines(max_width_cells: usize, size_px: u32) -> Vec<String> {
    let options = ImageOptions { max_width_cells: Some(max_width_cells), ..ImageOptions::default() };
    let dimensions = ImageDimensions { width_px: size_px, height_px: size_px };
    Image::new("AAAA", "image/png", ImageTheme::default(), options, Some(dimensions)).render(40)
}

fn kitty(data: &str, columns: usize, rows: usize, image_id: u32) -> String {
    let options = KittyEncodeOptions {
        columns: Some(columns),
        rows: Some(rows),
        image_id: Some(image_id),
        move_cursor: Some(false),
    };
    encode_kitty(data, &options)
}

#[test]
fn clears_reserved_kitty_image_rows_before_drawing_appended_image_placements() {
    let _guard = kitty_terminal();
    let terminal = LoggingTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&["before"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    let image_lines = image_lines(2, 20);
    let image_sequence = image_lines[0].clone();
    let mut lines = strings(&["before"]);
    lines.extend(image_lines);
    lines.push("after".to_string());
    component.borrow_mut().lines = lines;
    tui.request_render(false);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    assert!(
        writes.contains(&format!("\x1b[2K\r\n\x1b[2K\x1b[1A{image_sequence}\x1b[1B")),
        "reserved rows should be cleared before the image placement is drawn"
    );
    assert!(
        !writes.contains(&format!("{image_sequence}\r\n\x1b[2K")),
        "reserved row clears must not run after the image placement is drawn"
    );
    tui.stop(TuiStopOptions::default());
}

#[test]
fn falls_back_to_full_redraw_when_kitty_image_pre_clear_would_scroll() {
    let _guard = kitty_terminal();
    let terminal = LoggingTerminal::shared(40, 2);
    let tui = main_tui(&terminal);
    let component = test_component(&["before"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();
    let redraws_before_image = tui.full_redraws();
    terminal.borrow_mut().clear_writes();

    let mut lines = strings(&["before"]);
    lines.extend(image_lines(3, 30));
    lines.push("after".to_string());
    component.borrow_mut().lines = lines;
    tui.request_render(false);
    wait_for_render();

    assert!(tui.full_redraws() > redraws_before_image, "unsafe image pre-clear should force a full redraw");
    assert!(terminal.borrow().get_writes().contains("\x1b[2J"), "fallback should clear and fully redraw");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn reserves_kitty_image_rows_before_drawing_during_full_redraw_fallbacks() {
    let _guard = kitty_terminal();
    let terminal = LoggingTerminal::shared(40, 5);
    let tui = main_tui(&terminal);
    let component = test_component(&["l0", "l1", "l2", "l3", "l4"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();
    let redraws_before_image = tui.full_redraws();
    terminal.borrow_mut().clear_writes();

    let image_lines = image_lines(3, 30);
    let image_sequence = image_lines[0].clone();
    let mut lines = strings(&["l0", "l1", "l2", "l3", "l4"]);
    lines.extend(image_lines);
    lines.push("after".to_string());
    component.borrow_mut().lines = lines;
    tui.request_render(false);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    assert!(tui.full_redraws() > redraws_before_image, "scrolling image append should force a full redraw");
    assert!(
        writes.contains(&format!("\r\n\r\n\x1b[2A{image_sequence}\x1b[2B")),
        "full redraw should reserve visible image rows before drawing the placement"
    );
    assert!(
        !writes.contains(&format!("{image_sequence}\r\n\x1b[0m")),
        "full redraw must not write reserved padding rows after drawing the placement"
    );
    tui.stop(TuiStopOptions::default());
}

#[test]
fn does_not_use_cursor_up_placement_for_kitty_images_taller_than_the_viewport() {
    let _guard = kitty_terminal();
    let terminal = LoggingTerminal::shared(40, 5);
    let tui = main_tui(&terminal);
    let component = test_component(&["before"]);
    tui.add_child(erased(&component));
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    let image_lines = image_lines(6, 60);
    let image_sequence = image_lines[0].clone();
    let image_rows = image_lines.len();
    assert!(image_rows > terminal.borrow().rows(), "test image should exceed the viewport height");

    let mut lines = strings(&["before"]);
    lines.extend(image_lines);
    lines.push("after".to_string());
    component.borrow_mut().lines = lines;
    tui.request_render(true);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    assert!(writes.contains(&image_sequence), "image placement should be drawn");
    assert!(
        !writes.contains(&format!("\x1b[{}A{image_sequence}", image_rows - 1)),
        "taller-than-viewport images must keep the #4461 first-row placement path"
    );
    tui.stop(TuiStopOptions::default());
}

#[test]
fn deletes_changed_image_ids_before_drawing_moved_placements() {
    let _guard = global_state_lock();
    let terminal = LoggingTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    tui.add_child(erased(&component));

    let old_image = kitty("AAAA", 2, 2, 42);
    component.borrow_mut().lines = vec!["top".to_string(), old_image];
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    let new_image = kitty("BBBB", 2, 1, 42);
    component.borrow_mut().lines = vec![new_image.clone(), String::new()];
    tui.request_render(false);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    let delete_index = writes.find(&delete_kitty_image(42));
    let draw_index = writes.find(&new_image);
    assert!(delete_index.is_some(), "changed old image should be deleted");
    assert!(draw_index.is_some(), "new image should be drawn");
    assert!(delete_index < draw_index, "old image must be deleted before the new placement is drawn");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn redraws_image_lines_when_an_earlier_reserved_image_row_changes() {
    let _guard = global_state_lock();
    let terminal = LoggingTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    tui.add_child(erased(&component));

    let image = kitty("AAAA", 2, 2, 88);
    component.borrow_mut().lines = vec![String::new(), image.clone()];
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    component.borrow_mut().lines = vec!["covered".to_string(), image.clone()];
    tui.request_render(false);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    let delete_index = writes.find(&delete_kitty_image(88));
    let draw_index = writes.find(&image);
    assert!(delete_index.is_some(), "image should be deleted when a reserved row changes");
    assert!(draw_index.is_some(), "unchanged image line should be redrawn after deleting the placement");
    assert!(delete_index < draw_index, "old placement must be deleted before the image line is redrawn");
    assert!(!writes.contains("\x1b[2J"), "reserved row changes should not force a full redraw");
    tui.stop(TuiStopOptions::default());
}

#[test]
fn deletes_previously_rendered_image_ids_during_full_redraws() {
    let _guard = global_state_lock();
    let terminal = LoggingTerminal::shared(40, 10);
    let tui = main_tui(&terminal);
    let component = test_component(&[]);
    tui.add_child(erased(&component));

    component.borrow_mut().lines = vec![kitty("AAAA", 2, 2, 77)];
    tui.start();
    wait_for_render();
    terminal.borrow_mut().clear_writes();

    component.borrow_mut().lines = strings(&["plain text"]);
    tui.request_render(true);
    wait_for_render();

    let writes = terminal.borrow().get_writes();
    let delete_index = writes.find(&delete_kitty_image(77));
    let clear_index = writes.find("\x1b[2J");
    assert!(delete_index.is_some(), "previous image should be deleted during full redraw");
    assert!(clear_index.is_some(), "full redraw should clear the screen");
    assert!(delete_index < clear_index, "old image should be deleted before the screen is cleared");
    tui.stop(TuiStopOptions::default());
}
