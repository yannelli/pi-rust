//! Port of upstream `test/overlay-non-capturing.test.ts`, "rendering order": overlapping overlays
//! composite in focus order, and focusing an overlay brings it to the front.

mod overlay_common;

use overlay_common::*;

#[test]
fn focus_on_already_focused_overlay_bumps_visual_order() {
    let editor = focusable("EDITOR");
    let h = Harness::with_editor(20, 6, Some(cref(&editor)));
    h.start();
    let a_handle = h.show(static_overlay(&["A"]), corner(true));
    h.show(static_overlay(&["B"]), corner(true));
    a_handle.focus();
    h.show(static_overlay(&["C"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('C'));
    a_handle.focus();
    h.flush();
    assert_eq!(h.top_left(), Some('A'));
    assert!(a_handle.is_focused());
    h.stop();
}

#[test]
fn default_rendering_order_for_overlapping_overlays_follows_creation_order() {
    let h = Harness::with_editor(20, 6, None);
    h.start();
    h.show(static_overlay(&["A"]), corner(true));
    h.show(static_overlay(&["B"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    h.stop();
}

#[test]
fn focus_on_lower_overlay_renders_it_on_top() {
    let h = Harness::with_editor(20, 6, None);
    h.start();
    let lower = h.show(static_overlay(&["A"]), corner(true));
    h.show(static_overlay(&["B"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    lower.focus();
    h.flush();
    assert_eq!(h.top_left(), Some('A'));
    h.stop();
}

#[test]
fn focusing_middle_overlay_places_it_on_top_while_preserving_others_relative_order() {
    let h = Harness::with_editor(20, 6, None);
    h.start();
    h.show(static_overlay(&["A"]), corner(true));
    let middle = h.show(static_overlay(&["B"]), corner(true));
    let top = h.show(static_overlay(&["C"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('C'));
    middle.focus();
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    middle.hide();
    h.flush();
    assert_eq!(h.top_left(), Some('C'));
    top.hide();
    h.flush();
    assert_eq!(h.top_left(), Some('A'));
    h.stop();
}

#[test]
fn capturing_overlay_hidden_and_shown_again_renders_on_top_after_unhide() {
    let h = Harness::with_editor(20, 6, None);
    h.start();
    h.show(static_overlay(&["A"]), corner(true));
    let capturing = h.show(static_overlay(&["B"]), corner(false));
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    capturing.set_hidden(true);
    h.show(static_overlay(&["C"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('C'));
    capturing.set_hidden(false);
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    h.stop();
}

#[test]
fn unfocus_does_not_change_visual_order_until_another_overlay_is_focused() {
    let editor = focusable("EDITOR");
    let h = Harness::with_editor(20, 6, Some(cref(&editor)));
    h.start();
    let a = h.show(static_overlay(&["A"]), corner(true));
    let b = h.show(static_overlay(&["B"]), corner(true));
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    a.focus();
    h.flush();
    assert_eq!(h.top_left(), Some('A'));
    a.unfocus(None);
    h.flush();
    assert_eq!(h.top_left(), Some('A'));
    b.focus();
    h.flush();
    assert_eq!(h.top_left(), Some('B'));
    h.stop();
}
