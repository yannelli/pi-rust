//! Port of upstream `test/overlay-non-capturing.test.ts`, "focus management": base focus steals,
//! restore eligibility, and focus restore through overlay removals.

mod overlay_common;

use std::cell::Cell;
use std::rc::Rc;

use overlay_common::*;
use pi_tui::OverlayOptions;

const NONE: [&str; 0] = [];

/// Options whose visibility follows a shared flag.
fn visible_when(flag: &Rc<Cell<bool>>) -> OverlayOptions {
    let flag = flag.clone();
    OverlayOptions { visible: Some(Rc::new(move |_, _| flag.get())), ..Default::default() }
}

#[test]
fn handle_input_restores_focus_to_a_visible_focused_overlay_after_base_focus_steal() {
    let editor = focusable("EDITOR");
    let replacement = focusable("REPLACEMENT");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    assert!(focused(&overlay));
    h.focus(Some(cref(&replacement)));
    h.focus(Some(cref(&editor)));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), ["x"]);
    assert_eq!(inputs(&editor), NONE);
    assert!(focused(&overlay));
    h.stop();
}

#[test]
fn handle_input_restores_focus_to_explicitly_focused_raw_sub_overlay_after_base_focus_steal() {
    let editor = focusable("EDITOR");
    let controller = focusable("CONTROLLER");
    let sub_overlay = focusable("SUB");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&controller), OverlayOptions::default());
    let sub_handle = h.show(cref(&sub_overlay), non_capturing());
    sub_handle.focus();
    h.focus(Some(cref(&editor)));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&sub_overlay), ["x"]);
    assert_eq!(inputs(&controller), NONE);
    assert_eq!(inputs(&editor), NONE);
    h.stop();
}

#[test]
fn passive_non_capturing_overlay_does_not_regain_input_after_base_focus() {
    let editor = focusable("EDITOR");
    let passive = focusable("PASSIVE");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&passive), non_capturing());
    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&passive), NONE);
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn explicitly_focused_non_capturing_overlay_regains_input_after_base_focus_steal() {
    let editor = focusable("EDITOR");
    let overlay = focusable("NC");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.focus();
    h.focus(Some(cref(&editor)));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), ["x"]);
    assert_eq!(inputs(&editor), NONE);
    h.stop();
}

#[test]
fn unfocus_prevents_visible_overlay_from_regaining_input() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), OverlayOptions::default());
    handle.unfocus(None);
    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&overlay), NONE);
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn set_focus_null_explicitly_clears_visible_overlay_restore() {
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, None);
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    h.focus(None);
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), NONE);
    assert!(!focused(&overlay));
    h.stop();
}

#[test]
fn blocked_replacement_set_focus_null_resumes_the_visible_overlay() {
    let replacement = focusable("REPLACEMENT");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, None);
    replacement.borrow_mut().set_on_input(focus_on(&h.tui, "\r", None));
    overlay.borrow_mut().set_on_input(focus_on(&h.tui, "b", Some(cref(&replacement))));
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    h.send("b");
    h.flush();
    h.send("\r");
    h.send("x");
    h.flush();
    assert_eq!(inputs(&replacement), ["\r"]);
    assert_eq!(inputs(&overlay), ["b", "x"]);
    assert!(focused(&overlay));
    h.stop();
}

#[test]
fn temporarily_invisible_focused_overlay_falls_back_without_losing_restore_eligibility() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let visible = Rc::new(Cell::new(true));
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&overlay), visible_when(&visible));
    h.focus(Some(cref(&editor)));
    visible.set(false);
    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&overlay), NONE);
    visible.set(true);
    h.send("y");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&overlay), ["y"]);
    h.stop();
}

#[test]
fn temporarily_invisible_focused_overlay_with_null_pre_focus_restores_when_visible_again() {
    let overlay = focusable("OVERLAY");
    let visible = Rc::new(Cell::new(true));
    let h = Harness::with_editor(80, 24, None);
    h.start();
    h.show(cref(&overlay), visible_when(&visible));
    visible.set(false);
    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), NONE);
    visible.set(true);
    h.send("y");
    h.flush();
    assert_eq!(inputs(&overlay), ["y"]);
    h.stop();
}

#[test]
fn cyclic_overlay_pre_focus_ancestry_does_not_hang_focus_changes() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&overlay)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.focus();
    h.focus(Some(cref(&editor)));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&overlay), NONE);
    h.stop();
}

#[test]
fn handle_input_restores_the_focus_order_top_overlay_after_base_focus_steal() {
    let editor = focusable("EDITOR");
    let lower = focusable("LOWER");
    let upper = focusable("UPPER");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let lower_handle = h.show(cref(&lower), OverlayOptions::default());
    h.show(cref(&upper), OverlayOptions::default());
    lower_handle.focus();
    h.focus(Some(cref(&editor)));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&lower), ["x"]);
    assert_eq!(inputs(&upper), NONE);
    assert_eq!(inputs(&editor), NONE);
    h.stop();
}

#[test]
fn hide_overlay_does_not_reassign_focus_when_topmost_overlay_is_non_capturing() {
    let editor = focusable("EDITOR");
    let capturing = focusable("CAP");
    let non_capturing_overlay = focusable("NC");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&capturing), OverlayOptions::default());
    h.show(cref(&non_capturing_overlay), non_capturing());
    assert!(focused(&capturing));
    h.tui.hide_overlay();
    h.flush();
    assert!(focused(&capturing));
    h.stop();
}

#[test]
fn multiple_capturing_and_non_capturing_overlays_restore_focus_through_removals() {
    let editor = focusable("EDITOR");
    let c1 = focusable("C1");
    let n1 = focusable("N1");
    let c2 = focusable("C2");
    let n2 = focusable("N2");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let c1_handle = h.show(cref(&c1), OverlayOptions::default());
    h.show(cref(&n1), non_capturing());
    let c2_handle = h.show(cref(&c2), OverlayOptions::default());
    h.show(cref(&n2), non_capturing());
    assert!(focused(&c2));
    c2_handle.hide();
    h.flush();
    assert!(focused(&c1));
    c1_handle.hide();
    h.flush();
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn capturing_overlay_unfocus_on_topmost_capturing_overlay_falls_back_to_pre_focus() {
    let editor = focusable("EDITOR");
    let capturing = focusable("CAP");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&capturing), OverlayOptions::default());
    assert!(focused(&capturing));
    handle.unfocus(None);
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&capturing));
    h.stop();
}
