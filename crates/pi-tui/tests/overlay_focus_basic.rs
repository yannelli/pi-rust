//! Port of upstream `test/overlay-non-capturing.test.ts`, "focus management" (first part).

mod overlay_common;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use overlay_common::*;
use pi_tui::{OverlayHandle, OverlayOptions, ui_loop};

const NONE: [&str; 0] = [];

#[test]
fn non_capturing_overlay_preserves_focus_on_creation() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&overlay), non_capturing());
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&overlay));
    h.stop();
}

#[test]
fn focus_transfers_focus_to_the_overlay() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.focus();
    h.flush();
    assert!(!focused(&editor));
    assert!(focused(&overlay));
    assert!(handle.is_focused());
    h.stop();
}

#[test]
fn unfocus_restores_previous_focus() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.focus();
    handle.unfocus(None);
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&overlay));
    assert!(!handle.is_focused());
    h.stop();
}

#[test]
fn set_hidden_false_on_non_capturing_overlay_does_not_auto_focus() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.set_hidden(true);
    handle.set_hidden(false);
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&overlay));
    h.stop();
}

#[test]
fn hide_when_overlay_is_not_focused_does_not_change_focus() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.hide();
    h.flush();
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn hide_when_focused_restores_focus_correctly() {
    let editor = focusable("EDITOR");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let handle = h.show(cref(&overlay), non_capturing());
    handle.focus();
    handle.hide();
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&overlay));
    h.stop();
}

#[test]
fn capturing_overlay_removed_with_non_capturing_below_restores_focus_to_editor() {
    let editor = focusable("EDITOR");
    let non_capturing_overlay = focusable("NC");
    let capturing = focusable("CAP");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&non_capturing_overlay), non_capturing());
    let handle = h.show(cref(&capturing), OverlayOptions::default());
    assert!(focused(&capturing));
    handle.hide();
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&non_capturing_overlay));
    h.stop();
}

#[test]
fn sub_overlay_cleanup_then_hide_overlay_restores_focus_and_input_to_editor() {
    let editor = focusable("EDITOR");
    let timer = focusable("TIMER");
    let controller = focusable("CTRL");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let timer_handle = h.show(cref(&timer), non_capturing());
    h.show(cref(&controller), OverlayOptions::default());
    assert!(focused(&controller));
    assert!(!focused(&editor));
    timer_handle.hide();
    h.tui.hide_overlay();
    h.flush();
    assert!(focused(&editor));
    assert!(!focused(&controller));
    assert!(!focused(&timer));
    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&controller), NONE);
    assert_eq!(inputs(&timer), NONE);
    h.stop();
}

#[test]
fn removed_focused_child_overlay_does_not_become_parent_overlay_fallback() {
    let editor = focusable("EDITOR");
    let child = focusable("CHILD");
    let parent = focusable("PARENT");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    let child_handle = h.show(cref(&child), non_capturing());
    child_handle.focus();
    let parent_handle = h.show(cref(&parent), OverlayOptions::default());
    assert!(focused(&parent));

    child_handle.hide();
    parent_handle.hide();
    h.send("x");
    h.flush();

    assert_eq!(inputs(&editor), ["x"]);
    assert_eq!(inputs(&child), NONE);
    assert_eq!(inputs(&parent), NONE);
    assert!(focused(&editor));
    h.stop();
}

#[test]
fn microtask_deferred_sub_overlay_pattern_show_extension_custom_simulation_restores_focus() {
    let editor = focusable("EDITOR");
    let timer = focusable("TIMER");
    let controller = focusable("CTRL");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();

    // The factory shows the timer at once; the controller follows on a deferred loop task,
    // like the `.then()` callback of upstream `showExtensionCustom`.
    let timer_handle: Rc<RefCell<Option<OverlayHandle>>> = Rc::new(RefCell::new(None));
    *timer_handle.borrow_mut() = Some(h.show(cref(&timer), non_capturing()));
    let deferred_tui = h.tui.clone();
    let deferred_controller = cref(&controller);
    ui_loop::set_timeout(Duration::ZERO, move || {
        deferred_tui.show_overlay(deferred_controller, OverlayOptions::default());
    });
    let done_tui = h.tui.clone();
    let done_timer = timer_handle.clone();
    let done = move || {
        let handle = done_timer.borrow().clone().expect("timerHandle was not initialized");
        handle.hide();
        done_tui.hide_overlay();
    };

    ui_loop::run_pending().expect("deferred task");
    h.flush();

    assert!(focused(&controller));
    assert!(!focused(&editor));

    // Esc: cleanup and close, then the custom UI promise resolves.
    done();
    h.flush();

    assert!(focused(&editor), "editor should regain focus");
    assert!(!focused(&controller));
    assert!(!focused(&timer));

    h.send("x");
    h.flush();
    assert_eq!(inputs(&editor), ["x"], "editor should receive input after close");
    assert_eq!(inputs(&controller), NONE);
    h.stop();
}

#[test]
fn handle_input_redirection_skips_non_capturing_overlays_when_focused_overlay_becomes_invisible() {
    let editor = focusable("EDITOR");
    let fallback_capturing = focusable("FALLBACK");
    let non_capturing_overlay = focusable("NC");
    let primary = focusable("PRIMARY");
    let is_visible = Rc::new(RefCell::new(true));
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    h.start();
    h.show(cref(&fallback_capturing), OverlayOptions::default());
    h.show(cref(&non_capturing_overlay), non_capturing());
    let visible = is_visible.clone();
    h.show(
        cref(&primary),
        OverlayOptions { visible: Some(Rc::new(move |_, _| *visible.borrow())), ..Default::default() },
    );
    assert!(focused(&primary));
    *is_visible.borrow_mut() = false;
    h.send("x");
    h.flush();
    assert_eq!(inputs(&primary), NONE);
    assert_eq!(inputs(&non_capturing_overlay), NONE);
    assert_eq!(inputs(&fallback_capturing), ["x"]);
    assert!(focused(&fallback_capturing));
    h.stop();
}
