//! Port of upstream `test/overlay-non-capturing.test.ts`, "focus management": base focus
//! replacements that take focus from a capturing overlay and later give it back.

mod overlay_common;

use std::cell::RefCell;
use std::rc::Rc;

use overlay_common::*;
use pi_tui::{Container, OverlayOptions, OverlayUnfocusOptions};

const NONE: [&str; 0] = [];

/// Input hook that remounts `editor` as the only child of `base` and focuses it on Enter.
fn remount_editor_on_enter(
    h: &Harness,
    base: &Rc<RefCell<Container>>,
    editor: &Rc<RefCell<FocusableOverlay>>,
) -> impl FnMut(&str) + 'static {
    let tui = h.tui.downgrade();
    let (base, editor) = (base.clone(), cref(editor));
    move |data| {
        if data != "\r" {
            return;
        }
        base.borrow_mut().clear();
        base.borrow_mut().add_child(editor.clone());
        if let Some(tui) = tui.upgrade() {
            tui.set_focus(Some(editor.clone()));
        }
    }
}

#[test]
fn active_base_focus_replacement_receives_close_input_before_overlay_restore() {
    let editor = focusable("EDITOR");
    let replacement = focusable("REPLACEMENT");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    overlay.borrow_mut().set_on_input(focus_on(&h.tui, "b", Some(cref(&replacement))));
    replacement.borrow_mut().set_on_input(focus_on(&h.tui, "\r", Some(cref(&editor))));
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    assert!(focused(&overlay));
    h.send("b");
    h.flush();
    assert!(focused(&replacement));

    h.send("\r");
    h.flush();
    assert_eq!(inputs(&replacement), ["\r"]);
    assert_eq!(inputs(&overlay), ["b"]);
    assert!(focused(&overlay));

    h.send("x");
    h.flush();
    assert_eq!(inputs(&overlay), ["b", "x"]);
    h.stop();
}

#[test]
fn active_replacement_still_receives_input_when_it_is_another_overlay_pre_focus() {
    let editor = focusable("EDITOR");
    let replacement = focusable("REPLACEMENT");
    let passive = focusable("PASSIVE");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, Some(cref(&editor)));
    overlay.borrow_mut().set_on_input(focus_on(&h.tui, "b", Some(cref(&replacement))));
    replacement.borrow_mut().set_on_input(focus_on(&h.tui, "\r", Some(cref(&editor))));
    h.start();
    h.focus(Some(cref(&replacement)));
    h.show(cref(&passive), non_capturing());
    h.focus(Some(cref(&editor)));
    h.show(cref(&overlay), OverlayOptions::default());
    h.send("b");
    h.flush();
    assert!(focused(&replacement));

    h.send("1");
    h.send("\r");
    h.flush();
    assert_eq!(inputs(&replacement), ["1", "\r"]);
    assert_eq!(inputs(&overlay), ["b"]);
    assert!(focused(&overlay));
    h.stop();
}

#[test]
fn blocked_replacement_can_move_focus_internally_before_overlay_restore() {
    let h = Harness::new(80, 24);
    let base = pi_tui::component(Container::new());
    let editor = focusable("EDITOR");
    let first_replacement = focusable("FIRST");
    let second_replacement = focusable("SECOND");
    let overlay = focusable("OVERLAY");
    overlay.borrow_mut().set_on_input(focus_on(&h.tui, "b", Some(cref(&first_replacement))));
    first_replacement.borrow_mut().set_on_input(focus_on(&h.tui, "n", Some(cref(&second_replacement))));
    second_replacement.borrow_mut().set_on_input(remount_editor_on_enter(&h, &base, &editor));
    base.borrow_mut().add_child(cref(&editor));
    base.borrow_mut().add_child(cref(&first_replacement));
    base.borrow_mut().add_child(cref(&second_replacement));
    h.tui.add_child(base.clone());
    h.focus(Some(cref(&editor)));
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    h.send("b");
    h.flush();
    h.send("n");
    h.flush();
    h.send("2");
    h.send("\r");
    h.flush();

    assert_eq!(inputs(&overlay), ["b"]);
    assert_eq!(inputs(&first_replacement), ["n"]);
    assert_eq!(inputs(&second_replacement), ["2", "\r"]);
    assert!(focused(&overlay));
    h.stop();
}

#[test]
fn removed_replacement_restores_overlay_even_when_overlay_pre_focus_differs_from_next_focus() {
    let h = Harness::new(80, 24);
    let base = pi_tui::component(Container::new());
    let editor = focusable("EDITOR");
    let palette = focusable("PALETTE");
    let replacement = focusable("REPLACEMENT");
    let overlay = focusable("OVERLAY");
    overlay.borrow_mut().set_on_input(focus_on(&h.tui, "b", Some(cref(&replacement))));
    replacement.borrow_mut().set_on_input(remount_editor_on_enter(&h, &base, &editor));
    base.borrow_mut().add_child(cref(&editor));
    base.borrow_mut().add_child(cref(&palette));
    base.borrow_mut().add_child(cref(&replacement));
    h.tui.add_child(base.clone());
    h.focus(Some(cref(&palette)));
    h.start();
    h.show(cref(&overlay), OverlayOptions::default());
    h.send("b");
    h.flush();
    h.send("\r");
    h.send("x");
    h.flush();

    assert_eq!(inputs(&overlay), ["b", "x"]);
    assert_eq!(inputs(&replacement), ["\r"]);
    assert_eq!(inputs(&editor), NONE);
    assert!(focused(&overlay));
    h.stop();
}

#[test]
fn unfocus_target_releases_a_blocked_overlay_while_replacement_remains_focused() {
    let fallback = focusable("FALLBACK");
    let target = focusable("TARGET");
    let replacement = focusable("REPLACEMENT");
    let overlay = focusable("OVERLAY");
    let h = Harness::with_editor(80, 24, None);
    replacement.borrow_mut().set_on_input(focus_on(&h.tui, "\r", Some(cref(&fallback))));
    h.start();
    let overlay_handle = h.show(cref(&overlay), OverlayOptions::default());
    {
        let tui = h.tui.downgrade();
        let (replacement, target) = (cref(&replacement), cref(&target));
        let overlay_handle = overlay_handle.clone();
        overlay.borrow_mut().set_on_input(move |data| {
            if data != "b" {
                return;
            }
            if let Some(tui) = tui.upgrade() {
                tui.set_focus(Some(replacement.clone()));
            }
            overlay_handle.unfocus(Some(OverlayUnfocusOptions { target: Some(target.clone()) }));
        });
    }

    h.send("b");
    h.flush();
    assert!(focused(&replacement));
    h.send("\r");
    h.send("x");
    h.flush();

    assert_eq!(inputs(&overlay), ["b"]);
    assert_eq!(inputs(&replacement), ["\r"]);
    assert_eq!(inputs(&fallback), NONE);
    assert_eq!(inputs(&target), ["x"]);
    h.stop();
}
