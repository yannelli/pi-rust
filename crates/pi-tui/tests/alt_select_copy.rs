//! Port of the OSC 8 link, OSC 52, and injected copy cases of upstream `test/tui-alt-screen.test.ts`.

mod alt_select_common;
mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use alt_select_common::{Harness, await_copy, fixed_copy, osc52, recording_copy, wait_for_render};
use pi_tui::terminal_image::hyperlink;
use pi_tui::{CopyOutcome, TuiAltScreenOptions, component, render_fake_cursor, ui_loop};
use support::Text;

const SELECTABLE: &str = "alpha\nbeta\ngamma\ndelta";

fn drag_select_two_lines(h: &Harness, release: &str) {
    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;4;2M");
    h.send(release);
    wait_for_render();
}

#[test]
fn opens_an_osc_8_hyperlink_with_specific_or_generic_release_codes_but_not_on_drag() {
    let opened_urls = Rc::new(RefCell::new(Vec::<String>::new()));
    let opened = opened_urls.clone();
    let options = TuiAltScreenOptions {
        open_url: Some(Rc::new(move |url: &str| opened.borrow_mut().push(url.to_string()))),
        ..TuiAltScreenOptions::default()
    };
    let h = Harness::with_alt(20, 3, options);
    let url = "https://example.com/path?q=1";
    let bel_url = "https://example.com/bel";
    let emoji_url = "https://example.com/emoji";
    let text =
        format!("{}\n\x1b]8;;{bel_url}\x07link\x1b]8;;\x07\n{}", hyperlink("link", url), hyperlink("🙂", emoji_url));
    h.tui.add_child(component(Text::new(&text, 0, 0)));
    h.tui.start();
    wait_for_render();

    h.send("\x1b[<0;2;1M");
    h.send("\x1b[<3;2;1m");
    wait_for_render();
    assert_eq!(*opened_urls.borrow(), [url]);

    h.send("\x1b[<0;2;2M");
    h.send("\x1b[<0;2;2m");
    wait_for_render();
    assert_eq!(*opened_urls.borrow(), [url, bel_url]);

    h.send("\x1b[<0;2;3M");
    h.send("\x1b[<0;2;3m");
    wait_for_render();
    assert_eq!(*opened_urls.borrow(), [url, bel_url, emoji_url]);

    h.send("\x1b[<0;2;1M");
    h.send("\x1b[<32;4;1M");
    h.send("\x1b[<0;4;1m");
    wait_for_render();
    assert_eq!(*opened_urls.borrow(), [url, bel_url, emoji_url]);
    h.stop();
}

#[test]
fn selects_visible_text_with_the_mouse_and_copies_it_with_osc_52_after_a_generic_release() {
    let h = Harness::new(20, 4);
    h.tui.add_child(component(Text::new("\x1b[1mal\x1b[0mpha\nbeta\ngamma\ndelta", 0, 0)));
    h.tui.start();
    wait_for_render();

    drag_select_two_lines(&h, "\x1b[<3;4;2m");

    let clipboard_writes = h.writes_containing("\x1b]52;c;");
    assert!(h.any_write_contains(&osc52("alpha\nbeta")), "{clipboard_writes} clipboard writes");
    assert!(h.any_write_contains("\x1b[7m"));
    assert!(
        h.any_write_contains("al\x1b[0m\x1b[7mpha"),
        "selection inverse must be reapplied after a reset inside the selection"
    );
    assert!(h.viewport().iter().any(|line| line.contains("Copied!")));
    h.stop();
}

#[test]
fn keeps_the_selection_highlight_after_a_fake_cursor_cell() {
    let h = Harness::new(20, 4);
    h.tui.add_child(component(Text::new(&format!("> {}bcd", render_fake_cursor("a")), 0, 0)));
    h.tui.start();
    wait_for_render();

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;6;1M");
    h.send("\x1b[<3;6;1m");
    wait_for_render();

    assert!(
        h.any_write_contains("\x1b[27m\x1b[7mbcd"),
        "selection inverse must be reapplied after the fake cursor ends"
    );
    h.stop();
}

#[test]
fn uses_an_injected_copyselection_handler_instead_of_osc_52_and_reports_success() {
    let copied = Rc::new(RefCell::new(Vec::<String>::new()));
    let options = TuiAltScreenOptions { copy_selection: Some(recording_copy(&copied)), ..Default::default() };
    let h = Harness::with_alt(20, 4, options);
    h.tui.add_child(component(Text::new(SELECTABLE, 0, 0)));
    h.tui.start();
    wait_for_render();

    drag_select_two_lines(&h, "\x1b[<0;4;2m");

    assert_eq!(*copied.borrow(), ["alpha\nbeta"]);
    assert!(!h.any_write_contains("\x1b]52;c;"), "must not emit OSC 52 when a copySelection handler is provided");
    assert!(h.viewport().iter().any(|line| line.contains("Copied!")));
    h.stop();
}

#[test]
fn leaves_selections_visible_without_copying_when_copyonselect_is_disabled() {
    let copied = Rc::new(RefCell::new(Vec::<String>::new()));
    let options = TuiAltScreenOptions {
        copy_on_select: Some(false),
        copy_selection: Some(recording_copy(&copied)),
        ..Default::default()
    };
    let h = Harness::with_alt(20, 4, options);
    h.tui.add_child(component(Text::new(SELECTABLE, 0, 0)));
    h.tui.start();
    wait_for_render();

    drag_select_two_lines(&h, "\x1b[<0;4;2m");

    assert!(copied.borrow().is_empty());
    assert!(h.tui.has_active_selection());
    assert!(h.any_write_contains("\x1b[7m"));
    assert!(h.viewport().iter().all(|line| !line.contains("Copied!")));
    h.stop();
}

#[test]
fn copies_an_active_selection_programmatically() {
    let copied = Rc::new(RefCell::new(Vec::<String>::new()));
    let options = TuiAltScreenOptions { copy_selection: Some(recording_copy(&copied)), ..Default::default() };
    let h = Harness::with_alt(20, 4, options);
    h.tui.add_child(component(Text::new(SELECTABLE, 0, 0)));
    h.tui.start();
    wait_for_render();

    assert!(!h.tui.has_active_selection());
    assert!(!await_copy(h.tui.copy_active_selection_to_clipboard()));

    drag_select_two_lines(&h, "\x1b[<0;4;2m");

    assert_eq!(*copied.borrow(), ["alpha\nbeta"]);
    assert!(h.tui.has_active_selection());

    copied.borrow_mut().clear();
    assert!(await_copy(h.tui.copy_active_selection_to_clipboard()));
    wait_for_render();

    assert_eq!(*copied.borrow(), ["alpha\nbeta"]);
    assert!(h.viewport().iter().any(|line| line.contains("Copied!")));
    h.stop();
}

#[test]
fn flashes_an_error_when_the_injected_copyselection_handler_fails() {
    let options =
        TuiAltScreenOptions { copy_selection: Some(fixed_copy(CopyOutcome::Failed(None))), ..Default::default() };
    let h = Harness::with_alt(20, 4, options);
    h.tui.add_child(component(Text::new(SELECTABLE, 0, 0)));
    h.tui.start();
    wait_for_render();

    drag_select_two_lines(&h, "\x1b[<0;4;2m");

    assert!(h.viewport().iter().any(|line| line.contains("Copy failed")));
    assert!(!h.any_write_contains("\x1b]52;c;"), "must not emit OSC 52 when a copySelection handler is provided");
    h.stop();
}

#[test]
fn flashes_a_specific_error_returned_by_the_injected_copyselection_handler() {
    // Regression test for #9618.
    let message = "Clipboard unavailable: install wl-clipboard";
    let options = TuiAltScreenOptions {
        copy_on_select: Some(false),
        copy_selection: Some(fixed_copy(CopyOutcome::Failed(Some(message.to_string())))),
        ..Default::default()
    };
    let h = Harness::with_alt(80, 4, options);
    h.tui.add_child(component(Text::new(SELECTABLE, 0, 0)));
    h.tui.start();
    wait_for_render();

    drag_select_two_lines(&h, "\x1b[<0;4;2m");
    // Upstream wraps `tui.flash` to read the duration; the flash timer deadline shows it here.
    let before = Instant::now();
    assert!(!await_copy(h.tui.copy_active_selection_to_clipboard()));
    let after = Instant::now();
    wait_for_render();
    let flash_deadline = ui_loop::next_deadline().expect("flash timer");

    assert!(h.viewport().iter().any(|line| line.contains(message)));
    assert!(h.viewport().iter().all(|line| !line.contains("Copy failed")));
    let flash_duration_ms = 5000;
    assert!(flash_deadline.duration_since(after).as_millis() <= flash_duration_ms);
    assert!(flash_deadline.duration_since(before).as_millis() >= flash_duration_ms);
    h.stop();
}
