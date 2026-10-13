//! Port of upstream `test/overlay-non-capturing.test.ts`, "no-op guards" and
//! "focus cycle prevention".

mod overlay_common;

use overlay_common::*;
use pi_tui::{OverlayOptions, OverlayUnfocusOptions};

const NONE: [&str; 0] = [];

fn unfocus_to(target: Option<pi_tui::ComponentRef>) -> Option<OverlayUnfocusOptions> {
    Some(OverlayUnfocusOptions { target })
}

#[test]
fn focus_on_hidden_overlay_is_a_no_op() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.set_hidden(true);
    handle.focus();
    h.flush();
    assert!(focused(&editor));
    assert!(!handle.is_focused());
    h.stop();
}

#[test]
fn focus_after_hide_is_a_no_op() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.hide();
    handle.focus();
    h.flush();
    assert!(focused(&editor));
    assert!(!handle.is_focused());
    h.stop();
}

#[test]
fn unfocus_when_overlay_does_not_have_focus_is_a_no_op() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.unfocus(None);
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&overlay));
    h.stop();
}

#[test]
fn unfocus_with_null_pre_focus_clears_focus_and_does_not_route_input_back_to_overlay() {
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, None);
    h.start();
    let handle = h.show(cref(&overlay), OverlayOptions::default());
    assert!(focused(&overlay));
    handle.unfocus(None);
    assert!(!focused(&overlay));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), NONE);
    assert!(!handle.is_focused());
    h.stop();
}

#[test]
fn toggle_focus_between_non_capturing_overlays_then_unfocus_returns_to_editor() {
    let editor = focusable("EDITOR");
    let a = focusable("A");
    let b = focusable("B");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let a_handle = h.show(cref(&a), non_capturing());
    let b_handle = h.show(cref(&b), non_capturing());
    a_handle.focus();
    b_handle.focus();
    a_handle.focus();
    a_handle.unfocus(None);
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&a));
    assert!(!focused(&b));
    h.stop();
}

#[test]
fn explicit_unfocus_target_supports_cycling_between_three_overlays_and_editor() {
    let editor = focusable("EDITOR");
    let a = focusable("A");
    let b = focusable("B");
    let c = focusable("C");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let a_handle = h.show(cref(&a), OverlayOptions::default());
    let b_handle = h.show(cref(&b), OverlayOptions::default());
    let c_handle = h.show(cref(&c), OverlayOptions::default());

    a_handle.focus();
    h.send("a");
    h.flush();
    b_handle.focus();
    h.send("b");
    h.flush();
    c_handle.focus();
    h.send("c");
    h.flush();
    c_handle.unfocus(unfocus_to(Some(cref(&editor))));
    h.send("e");
    h.flush();
    a_handle.focus();
    h.send("A");
    h.flush();
    a_handle.unfocus(unfocus_to(Some(cref(&editor))));
    h.send("E");
    h.flush();

    assert_eq!(inputs(&a), ["a", "A"]);
    assert_eq!(inputs(&b), ["b"]);
    assert_eq!(inputs(&c), ["c"]);
    assert_eq!(inputs(&editor), ["e", "E"]);
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn explicit_null_unfocus_target_clears_focus_without_restoring_overlays() {
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, None);
    h.start();
    let handle = h.show(cref(&overlay), OverlayOptions::default());
    handle.unfocus(unfocus_to(None));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), NONE);
    assert!(!handle.is_focused());
    h.stop();
}

#[test]
fn hiding_focused_overlay_falls_back_to_next_visual_frontmost_overlay() {
    let editor = focusable("EDITOR");
    let a = focusable("A");
    let b = focusable("B");
    let c = focusable("C");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let a_handle = h.show(cref(&a), OverlayOptions::default());
    let b_handle = h.show(cref(&b), OverlayOptions::default());
    h.show(cref(&c), OverlayOptions::default());
    a_handle.focus();
    b_handle.focus();
    b_handle.set_hidden(true);
    h.send("x");
    h.flush();
    assert_eq!(inputs(&a), ["x"]);
    assert_eq!(inputs(&c), NONE);
    assert!(focused(&a));
    h.stop();
}
