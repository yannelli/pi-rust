//! Port of the multi-click, focus loss, flash, auto-scroll, and grapheme selection cases of
//! upstream `test/tui-alt-screen.test.ts`.

mod alt_select_common;
mod support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use alt_select_common::{Harness, numbered_lines, osc52, recording_copy, wait_for_render};
use pi_tui::{TuiAltScreenOptions, component, ui_loop};
use support::Text;

const SELECTABLE: &str = "alpha\nbeta\ngamma\ndelta";

fn harness_with_text(columns: usize, rows: usize, text: &str) -> Harness {
    let h = Harness::new(columns, rows);
    h.tui.add_child(component(Text::new(text, 0, 0)));
    h.tui.start();
    wait_for_render();
    h
}

#[test]
fn does_not_append_whitespace_to_double_click_word_highlighting() {
    let h = harness_with_text(20, 1, "foo  bar");

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<0;1;1m");
    h.send("\x1b[<0;3;1M");
    wait_for_render();

    assert!(h.any_write_contains("foo\x1b[27m"));
    h.stop();
}

#[test]
fn coalesces_segments_joined_by_iterm2_word_characters_for_double_click_word_selection() {
    for (line, needle) in [
        ("extensions/starline/fixed-editor/compositor.ts", "starline"),
        ("earendil-works/pi-tui", "works"),
        ("~/Documents/pi/extension/pi-temp-session", "Documents"),
        ("~/.pi/agent/settings.json", "agent"),
        ("~\\Documents\\pi", "Documents"),
        ("foo+bar", "bar"),
    ] {
        let copied = Rc::new(RefCell::new(Vec::<String>::new()));
        let options = TuiAltScreenOptions { copy_selection: Some(recording_copy(&copied)), ..Default::default() };
        let h = Harness::with_alt(80, 1, options);
        h.tui.add_child(component(Text::new(line, 0, 0)));
        h.tui.start();
        wait_for_render();

        let one_based_click_column = line.find(needle).expect("needle") + 1;
        h.send(&format!("\x1b[<0;{one_based_click_column};1M"));
        h.send(&format!("\x1b[<0;{one_based_click_column};1m"));
        h.send(&format!("\x1b[<0;{one_based_click_column};1M"));
        h.send(&format!("\x1b[<0;{one_based_click_column};1m"));
        wait_for_render();

        assert_eq!(*copied.borrow(), [line]);
        h.stop();
    }
}

#[test]
fn highlights_a_complete_whitespace_segment_during_a_word_drag() {
    let h = harness_with_text(20, 1, "foo  bar");

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<0;1;1m");
    h.send("\x1b[<0;2;1M");
    h.send("\x1b[<32;4;1M");
    wait_for_render();

    assert!(h.any_write_contains("foo  \x1b[27m"));
    h.stop();
}

#[test]
fn selects_whole_words_on_double_click_extends_word_drags_and_selects_lines_on_triple_click() {
    let h = harness_with_text(20, 2, "zero alpha beta\ngamma delta");

    // The second click lands on a different character in alpha.
    h.send("\x1b[<0;6;1M");
    h.send("\x1b[<0;6;1m");
    h.send("\x1b[<0;10;1M");
    h.send("\x1b[<0;10;1m");
    wait_for_render();
    assert!(h.any_write_contains(&osc52("alpha")));

    // A double-click drag includes each word touched, rather than partial words.
    h.send("\x1b[<0;12;1M");
    h.send("\x1b[<0;12;1m");
    h.send("\x1b[<0;14;1M");
    h.send("\x1b[<32;3;2M");
    h.send("\x1b[<0;3;2m");
    wait_for_render();
    assert!(h.any_write_contains(&osc52("beta\ngamma")));

    h.send("\x1b[<0;7;2M");
    h.send("\x1b[<0;7;2m");
    h.send("\x1b[<0;9;2M");
    h.send("\x1b[<0;9;2m");
    h.send("\x1b[<0;11;2M");
    h.send("\x1b[<0;11;2m");
    wait_for_render();
    assert!(h.any_write_contains(&osc52("gamma delta")));
    h.stop();
}

#[test]
fn does_not_repaint_idle_or_zero_width_selections_on_focus_loss() {
    let h = harness_with_text(20, 4, SELECTABLE);
    let clipboard_write_count = || h.writes_containing("\x1b]52;c;");

    let idle_write_count = h.write_count();
    h.send("\x1b[O");
    h.send("\x1b[I");
    wait_for_render();
    assert_eq!(h.write_count(), idle_write_count);

    // A completed click leaves a zero-width anchor, but later orphaned drag/release events must not extend it.
    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<0;1;1m");
    h.send("\x1b[<32;4;2M");
    h.send("\x1b[<0;4;2m");
    wait_for_render();
    assert_eq!(clipboard_write_count(), 0);

    // Losing focus after a press without a drag cancels the press without repainting.
    h.send("\x1b[<0;1;3M");
    wait_for_render();
    let pressed_write_count = h.write_count();
    h.send("\x1b[O");
    h.send("\x1b[I");
    wait_for_render();
    assert_eq!(h.write_count(), pressed_write_count);
    h.send("\x1b[<32;4;2M");
    h.send("\x1b[<0;4;2m");
    wait_for_render();
    assert_eq!(clipboard_write_count(), 0);
    assert!(h.any_write_contains("\x1b[?1004h"));

    h.stop();
    assert!(h.any_write_contains("\x1b[?1004l"));
}

#[test]
fn remembers_the_last_focus_report() {
    let h = harness_with_text(20, 4, SELECTABLE);
    assert_eq!(h.tui.terminal_focused(), None);
    h.send("\x1b[O");
    wait_for_render();
    assert_eq!(h.tui.terminal_focused(), Some(false));
    h.send("\x1b[I");
    wait_for_render();
    assert_eq!(h.tui.terminal_focused(), Some(true));
    h.stop();
}

#[test]
fn clears_an_active_visible_selection_on_focus_loss_and_ignores_orphan_events() {
    let h = harness_with_text(20, 4, SELECTABLE);

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;4;2M");
    wait_for_render();
    let focus_loss_event_count = h.event_count();
    h.send("\x1b[O");
    h.send("\x1b[I");
    wait_for_render();
    let focus_loss_writes = h.writes_since(focus_loss_event_count);
    assert!(focus_loss_writes.contains("alpha"));
    assert!(focus_loss_writes.contains("beta"));
    assert!(!focus_loss_writes.contains("\x1b[7m"));

    h.send("\x1b[<32;4;2M");
    h.send("\x1b[<0;4;2m");
    wait_for_render();
    assert!(!h.any_write_contains("\x1b]52;c;"));
    h.stop();
}

#[test]
fn retains_a_completed_visible_selection_across_focus_changes() {
    let h = harness_with_text(20, 4, SELECTABLE);

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;4;2M");
    h.send("\x1b[<0;4;2m");
    wait_for_render();
    let completed_write_count = h.write_count();
    h.send("\x1b[O");
    h.send("\x1b[I");
    wait_for_render();
    assert_eq!(h.write_count(), completed_write_count);

    let redraw_event_count = h.event_count();
    h.tui.render_now(true).expect("render");
    let redraw_writes = h.writes_since(redraw_event_count);
    assert!(redraw_writes.contains("alpha"));
    assert!(redraw_writes.contains("beta"));
    assert!(redraw_writes.contains("\x1b[7m"));
    h.stop();
}

#[test]
fn stacks_flash_messages_and_collapses_them_as_they_expire() {
    let h = harness_with_text(20, 4, "one\ntwo\nthree\nfour");

    h.tui.flash("First", Some(Duration::from_millis(80)));
    h.tui.flash("Second", Some(Duration::from_secs(30)));
    // Render without running the loop, so no expiry timer can fire before this check.
    h.tui.render_now(true).expect("render");
    let viewport = h.xterm_viewport();
    assert!(viewport[0].ends_with(" First "), "{viewport:?}");
    assert!(viewport[1].ends_with(" Second "), "{viewport:?}");

    let started = std::time::Instant::now();
    while h.xterm_viewport().iter().any(|line| line.contains("First")) {
        assert!(started.elapsed() < Duration::from_secs(10), "the first flash never expired");
        ui_loop::run_once(Some(Duration::from_millis(20))).expect("loop");
    }
    let viewport = h.xterm_viewport();
    assert!(viewport[0].ends_with(" Second "), "{viewport:?}");
    assert!(!viewport.iter().any(|line| line.contains("First")));
    h.stop();
}

#[test]
fn auto_scrolls_and_extends_a_drag_selection_held_at_the_viewport_edge() {
    let h = harness_with_text(20, 4, &numbered_lines(10));
    assert_eq!(h.tui.viewport_top(), 6);

    h.send("\x1b[<0;1;3M");
    h.send("\x1b[<32;1;1M");
    ui_loop::run_for(Duration::from_millis(130)).expect("loop");
    wait_for_render();

    let selection_top = h.tui.viewport_top();
    assert!(selection_top < 6, "expected auto-scroll above row 6, got {selection_top}");
    h.send("\x1b[<0;1;1m");
    wait_for_render();

    let mut selected_lines: Vec<String> =
        (0..8 - selection_top).map(|i| format!("line {}", selection_top + i + 1)).collect();
    selected_lines.push("l".to_string());
    let expected = osc52(&selected_lines.join("\n"));
    assert!(h.any_write_contains(&expected), "{} clipboard writes", h.writes_containing("\x1b]52;c;"));
    h.stop();
}

#[test]
fn snaps_mouse_selection_to_cjk_emoji_and_combining_grapheme_boundaries() {
    let h = harness_with_text(20, 2, "A界🙂e\u{301}Z");

    let wide_selection = osc52("界🙂");
    h.send("\x1b[<0;3;1M");
    h.send("\x1b[<32;4;1M");
    h.send("\x1b[<0;4;1m");
    wait_for_render();
    assert_eq!(h.writes_containing(&wide_selection), 1);

    h.send("\x1b[<0;5;1M");
    h.send("\x1b[<32;2;1M");
    h.send("\x1b[<0;2;1m");
    wait_for_render();
    assert_eq!(h.writes_containing(&wide_selection), 2);

    let combining_selection = osc52("e\u{301}Z");
    h.send("\x1b[<0;6;1M");
    h.send("\x1b[<32;7;1M");
    h.send("\x1b[<0;7;1m");
    wait_for_render();
    assert!(h.any_write_contains(&combining_selection));
    h.stop();
}
