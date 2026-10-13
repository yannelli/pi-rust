//! Port of upstream `tui-alt-screen.test.ts`: viewport, dock, and jump-to-end indicator cases.

mod alt_view_common;
mod layout_common;
mod support;

use std::cell::Cell;
use std::rc::Rc;

use alt_view_common::{Harness, column_of, numbered_lines, scroll_view};
use layout_common::{entry, text};
use pi_tui::components::ScrollFollow;
use pi_tui::{
    Component, ComponentRef, OverlayOptions, ScrollViewOptions, ScrollViewScrollbar, StackChild, StackOptions,
    TuiAltScreenOptions, VStack, component,
};

fn indicator(label: &'static str) -> TuiAltScreenOptions {
    TuiAltScreenOptions { scroll_to_end_indicator: Some(Rc::new(move || label.to_string())), ..Default::default() }
}

fn follow_primary() -> ScrollViewOptions {
    ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..ScrollViewOptions::default() }
}

/// Transcript that grows over a dock of `editor` and `footer`.
fn transcript_over_dock(transcript: ComponentRef) -> ComponentRef {
    component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).min(1).into(), entry(text("editor\nfooter")).auto().min(1).into()],
        StackOptions::default(),
    ))
}

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

#[test]
fn renders_a_terminal_height_viewport_and_preserves_manual_scroll_position() {
    let h = Harness::new(20, 4);
    let content = text(&numbered_lines(10));
    h.tui.add_child(content.clone());
    h.tui.start();
    h.wait();

    assert_eq!(h.viewport(), strings(&["line 7", "line 8", "line 9", "line 10"]));
    assert!(h.tui.is_following_output());

    h.send("\x1b[<64;1;1M");
    h.wait();
    assert_eq!(h.viewport(), strings(&["line 6", "line 7", "line 8", "line 9"]));
    assert_eq!(h.tui.viewport_top(), 5);
    assert!(!h.tui.is_following_output());

    content.borrow_mut().set_text(&numbered_lines(12));
    h.tui.request_render(false);
    h.wait();
    assert_eq!(h.viewport(), strings(&["line 6", "line 7", "line 8", "line 9"]));

    h.stop();
}

#[test]
fn shows_a_clickable_jump_to_end_indicator_on_the_transcripts_last_row_while_scrolled_up() {
    let h = Harness::with_options(30, 6, indicator("\x1b[7m ↓ Jump to end \x1b[27m"));
    let (transcript, state) = scroll_view(text(&numbered_lines(8)), follow_primary());
    h.tui.set_layout_root(Some(transcript_over_dock(transcript)));
    h.tui.start();
    h.wait();
    assert!(!h.viewport_has("Jump to end"));

    h.send("\x1b[<64;1;1M");
    h.wait();
    assert!(!state.is_following_end());
    assert_eq!(h.raw_viewport()[3], "line 7  ↓ Jump to end         ");
    assert_eq!(h.viewport()[4], "editor");

    // Pressing next to the label starts a selection instead of jumping.
    h.send("\x1b[<0;2;4M");
    h.send("\x1b[<0;2;4m");
    h.wait();
    assert!(!state.is_following_end());

    h.send("\x1b[<0;15;4M");
    h.send("\x1b[<0;15;4m");
    h.wait();
    assert!(state.is_following_end());
    assert_eq!(h.viewport(), strings(&["line 5", "line 6", "line 7", "line 8", "editor", "footer"]));
    h.stop();
}

#[test]
fn keeps_the_jump_to_end_indicator_centered_as_the_auto_scrollbar_hides_and_reappears() {
    // Regression test for #9136: auto scrollbar visibility must not move the indicator.
    let label = " ↓ Jump to latest message · End ";
    let h = Harness::with_options(80, 6, indicator(label));
    let options = ScrollViewOptions {
        scrollbar: ScrollViewScrollbar::Auto,
        scrollbar_hide_delay_ms: Some(0),
        ..follow_primary()
    };
    let (transcript, state) = scroll_view(text(&numbered_lines(20)), options);
    h.tui.set_layout_root(Some(transcript));
    h.tui.start();
    h.wait();

    // Scrolling over the track keeps the scrollbar visible until the pointer leaves.
    h.send("\x1b[<64;80;1M");
    h.wait();
    assert!(state.is_scrollbar_visible());
    assert!(!state.is_following_end());
    let scroll_top = state.scroll_top();
    let visible_column = column_of(&h.raw_viewport()[5], label);

    // Leaving the track lets the auto-hide timer expire without changing the content.
    h.send("\x1b[<35;79;1M");
    h.wait();
    assert!(!state.is_scrollbar_visible());
    assert_eq!(state.scroll_top(), scroll_top);
    let hidden_column = column_of(&h.raw_viewport()[5], label);

    h.send("\x1b[<35;80;1M");
    h.wait();
    assert!(state.is_scrollbar_visible());
    assert_eq!(state.scroll_top(), scroll_top);
    let revealed_column = column_of(&h.raw_viewport()[5], label);

    assert_eq!([visible_column, hidden_column, revealed_column], [Some(24); 3]);
    h.stop();
}

#[test]
fn leaves_the_scrollbar_visible_and_clickable_when_the_jump_to_end_indicator_spans_the_transcript() {
    // Regression coverage for #9136: centering must not paint or capture clicks over the scrollbar.
    let label: &'static str = "↓".repeat(30).leak();
    let h = Harness::with_options(30, 6, indicator(label));
    let options = ScrollViewOptions { scrollbar: ScrollViewScrollbar::Always, ..follow_primary() };
    let (transcript, state) = scroll_view(text(&numbered_lines(12)), options);
    h.tui.set_layout_root(Some(transcript_over_dock(transcript)));
    h.tui.start();
    h.wait();

    h.send("\x1b[<64;1;1M");
    h.wait();
    assert!(!state.is_following_end());

    // The indicator must not intercept a press on the scrollbar's last column.
    assert_eq!(h.raw_viewport()[3], format!("{}┃", "↓".repeat(29)));
    h.send("\x1b[<0;30;4M");
    h.send("\x1b[<0;30;4m");
    h.wait();
    assert!(!state.is_following_end());
    h.stop();
}

#[test]
fn never_shows_the_jump_to_end_indicator_for_a_primary_scroll_view_without_follow_end() {
    let h = Harness::with_options(30, 3, indicator(" ↓ Jump to end "));
    let options = ScrollViewOptions { primary: true, ..ScrollViewOptions::default() };
    let (transcript, state) = scroll_view(text("one\ntwo\nthree\nfour\nfive"), options);
    h.tui.set_layout_root(Some(transcript));
    h.tui.start();
    h.wait();

    assert!(!state.is_following_end());
    assert!(!h.viewport_has("Jump to end"));
    h.stop();
}

#[test]
fn keeps_an_explicit_dock_fixed_while_the_transcript_scrolls() {
    let h = Harness::new(20, 6);
    let transcript_text = text(&numbered_lines(8));
    let (transcript, state) = scroll_view(transcript_text.clone(), follow_primary());
    let dock: ComponentRef = component(VStack::new(
        vec![StackChild::Component(text("editor")), StackChild::Component(text("footer"))],
        StackOptions::default(),
    ));
    h.tui.set_layout_root(Some(component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).min(1).into(), entry(dock).auto().min(1).into()],
        StackOptions::default(),
    ))));
    h.tui.start();
    h.wait();

    assert_eq!(h.viewport(), strings(&["line 5", "line 6", "line 7", "line 8", "editor", "footer"]));

    // Wheel over the dock falls back to the primary transcript scroll view.
    h.send("\x1b[<64;1;6M");
    h.wait();
    assert_eq!(h.viewport(), strings(&["line 4", "line 5", "line 6", "line 7", "editor", "footer"]));
    assert!(!state.is_following_end());

    transcript_text.borrow_mut().set_text(&numbered_lines(10));
    h.tui.request_render(false);
    h.wait();
    assert_eq!(h.viewport(), strings(&["line 4", "line 5", "line 6", "line 7", "editor", "footer"]));

    h.tui.scroll_to_bottom();
    h.wait();
    assert_eq!(h.viewport(), strings(&["line 7", "line 8", "line 9", "line 10", "editor", "footer"]));
    h.stop();
}

/// Upstream `Text("overlay")` with an `invalidate` override that records the call.
struct InvalidateProbe(Rc<Cell<bool>>);

impl Component for InvalidateProbe {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec!["overlay".to_string()]
    }

    fn invalidate(&mut self) {
        self.0.set(true);
    }
}

#[test]
fn invalidates_overlays_with_an_explicit_layout_root() {
    let h = Harness::new(80, 24);
    let invalidated = Rc::new(Cell::new(false));
    h.tui.set_layout_root(Some(text("root")));
    h.tui.show_overlay(component(InvalidateProbe(invalidated.clone())), OverlayOptions::default());

    h.tui.invalidate();

    assert!(invalidated.get());
    h.stop();
}
