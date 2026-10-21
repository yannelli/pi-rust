//! Port of the wheel, mouse-aware component, leave-alt-mode, overlay, and transcript search cases
//! of upstream `test/tui-alt-screen.test.ts`.

mod alt_select_common;
mod support;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use alt_select_common::{FnComponent, Harness, TerminalEvent, input_overlay, numbered_lines, wait_for_render};
use pi_tui::component::same_component_opt;
use pi_tui::{
    ComponentRef, OverlayAnchor, OverlayOptions, SizeValue, TuiMouseEventResult, TuiMouseEventType, component,
};
use support::{MouseRegion, Text};

fn text(value: &str) -> ComponentRef {
    component(Text::new(value, 0, 0))
}

#[test]
fn ignores_horizontal_trackpad_wheel_events() {
    let h = Harness::new(20, 4);
    h.tui.add_child(text(&numbered_lines(8)));
    h.tui.start();
    wait_for_render();

    h.send("\x1b[<66;1;1M");
    h.send("\x1b[<67;1;1M");
    wait_for_render();
    assert_eq!(h.tui.viewport_top(), 4);
    let viewport: Vec<String> = h.viewport().iter().map(|line| line.trim_end().to_string()).collect();
    assert_eq!(viewport, ["line 5", "line 6", "line 7", "line 8"]);
    h.stop();
}

#[test]
fn dispatches_clicks_to_nested_mouse_regions_without_breaking_drag_selection() {
    let h = Harness::new(20, 2);
    let clicks = Rc::new(Cell::new(0));
    let counter = clicks.clone();
    h.tui.add_child(component(MouseRegion::new(text("clickable\nselectable"), move |event| {
        if event.kind != TuiMouseEventType::Click {
            return None;
        }
        counter.set(counter.get() + 1);
        Some(TuiMouseEventResult::handled())
    })));
    h.tui.start();
    wait_for_render();

    h.send("\x1b[<0;2;1M");
    h.send("\x1b[<0;2;1m");
    wait_for_render();
    assert_eq!(clicks.get(), 1);

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;4;2M");
    h.send("\x1b[<0;4;2m");
    wait_for_render();
    assert_eq!(clicks.get(), 1);
    assert!(h.any_write_contains("\x1b]52;c;"));
    h.stop();
}

#[test]
fn focuses_and_captures_drag_gestures_for_mouse_aware_components() {
    let h = Harness::new(20, 2);
    let events = Rc::new(RefCell::new(Vec::<TuiMouseEventType>::new()));
    let recorded = events.clone();
    let control: ComponentRef = component(FnComponent::fixed(&["control"]).with_mouse(move |event| {
        recorded.borrow_mut().push(event.kind);
        let press = event.kind == TuiMouseEventType::Press;
        Some(TuiMouseEventResult { handled: true, capture: press, focus: press, render: None })
    }));
    h.tui.add_child(control.clone());
    h.tui.start();
    wait_for_render();

    h.send("\x1b[<0;1;1M");
    h.send("\x1b[<32;5;2M");
    h.send("\x1b[<0;5;2m");
    wait_for_render();

    use TuiMouseEventType::{Drag, Press, Release};
    assert_eq!(*events.borrow(), [Press, Drag, Release]);
    assert!(same_component_opt(h.tui.get_focused_component().as_ref(), Some(&control)));
    h.stop();
}

#[test]
fn reports_consecutive_click_counts_to_component_owned_controls() {
    let h = Harness::new(20, 1);
    let click_counts = Rc::new(RefCell::new(Vec::<u32>::new()));
    let counts = click_counts.clone();
    h.tui.add_child(component(FnComponent::fixed(&["control"]).with_mouse(move |event| match event.kind {
        TuiMouseEventType::Press => Some(TuiMouseEventResult::handled()),
        TuiMouseEventType::Click => {
            counts.borrow_mut().push(event.click_count.unwrap_or(0));
            Some(TuiMouseEventResult::handled())
        }
        _ => None,
    })));
    h.tui.start();
    wait_for_render();

    for _ in 0..3 {
        h.send("\x1b[<0;1;1M");
        h.send("\x1b[<0;1;1m");
    }
    wait_for_render();
    assert_eq!(*click_counts.borrow(), [1, 2, 3]);
    h.stop();
}

#[test]
fn does_not_rerender_for_handled_no_op_pointer_motion() {
    let h = Harness::new(20, 2);
    let render_count = Rc::new(Cell::new(0));
    let renders = render_count.clone();
    let target = FnComponent::lines(move |_| {
        renders.set(renders.get() + 1);
        vec!["hover target".to_string()]
    });
    h.tui.add_child(component(
        target.with_mouse(|event| (event.kind == TuiMouseEventType::Move).then(TuiMouseEventResult::handled)),
    ));
    h.tui.start();
    wait_for_render();
    let rendered_before_motion = render_count.get();
    let writes_before_motion = h.write_count();

    h.send("\x1b[<35;1;1M");
    wait_for_render();
    assert_eq!(render_count.get(), rendered_before_motion);
    assert_eq!(h.write_count(), writes_before_motion);
    h.stop();
}

#[test]
fn lets_mouse_aware_components_consume_wheel_events_before_viewport_scrolling() {
    let h = Harness::new(20, 3);
    let wheel_events = Rc::new(Cell::new(0));
    let counter = wheel_events.clone();
    h.tui.add_child(component(MouseRegion::new(text(&numbered_lines(8)), move |event| {
        if event.kind != TuiMouseEventType::Wheel {
            return None;
        }
        counter.set(counter.get() + 1);
        Some(TuiMouseEventResult::handled())
    })));
    h.tui.start();
    wait_for_render();
    let viewport_top = h.tui.viewport_top();

    h.send("\x1b[<64;1;1M");
    wait_for_render();
    assert_eq!(wheel_events.get(), 1);
    assert_eq!(h.tui.viewport_top(), viewport_top);
    h.stop();
}

#[test]
fn restores_keyboard_state_before_leaving_alt_mode_and_prints_the_full_document() {
    let h = Harness::new(20, 3);
    h.tui.add_child(text("first\nsecond\nthird\nfourth\nfifth\nsixth"));
    h.tui.start();
    wait_for_render();
    h.stop();

    let events = h.events();
    let write_index = |needle: &str| {
        events.iter().position(|event| matches!(event, TerminalEvent::Write(data) if data.contains(needle)))
    };
    let start_index = events.iter().position(|event| *event == TerminalEvent::Start).expect("start");
    let stop_index = events.iter().position(|event| *event == TerminalEvent::Stop).expect("stop");
    let alt_screen_enter_index = write_index("\x1b[?1049h").expect("alt screen enter");
    let mouse_disable_index = write_index("\x1b[?1006l").expect("mouse disable");
    let main_screen_restore_index = write_index("\x1b[?1049l").expect("main screen restore");
    assert!(alt_screen_enter_index < start_index);
    assert!(mouse_disable_index < stop_index);
    assert!(main_screen_restore_index > stop_index);

    let TerminalEvent::Write(restore) = &events[main_screen_restore_index] else { panic!("restore is a write") };
    for line in ["first", "second", "third", "fourth", "fifth", "sixth"] {
        assert!(restore.contains(line), "missing {line}");
    }
    assert!(restore.find("first") < restore.find("sixth"));
}

/// #10769
#[test]
fn scrolls_the_transcript_on_wheel_input_a_focused_overlay_does_not_handle() {
    let h = Harness::new(20, 6);
    h.tui.add_child(text(&numbered_lines(12)));
    let overlay = input_overlay();
    h.tui.start();
    wait_for_render();
    let top_before = h.tui.viewport_top();
    let options = OverlayOptions {
        anchor: Some(OverlayAnchor::TopLeft),
        width: Some(SizeValue::Absolute(10)),
        ..Default::default()
    };
    h.tui.show_overlay(overlay.clone(), options);
    wait_for_render();
    assert!(overlay.borrow().focused);

    h.send("\x1b[<64;3;1M");
    wait_for_render();
    assert!(h.tui.viewport_top() < top_before);
    assert!(overlay.borrow().inputs.is_empty());
    h.stop();
}

#[test]
fn gives_viewport_keys_to_a_focused_overlay() {
    let h = Harness::new(20, 6);
    h.tui.add_child(text(&numbered_lines(12)));
    let overlay = input_overlay();
    h.tui.start();
    wait_for_render();
    let top_before = h.tui.viewport_top();
    let handle = h.tui.show_overlay(overlay.clone(), OverlayOptions::default());
    wait_for_render();
    assert!(overlay.borrow().focused);

    let keys = ["\x1b[5~", "\x1b[6~", "\x1bOH", "\x1bOF"];
    for key in keys {
        h.send(key);
    }
    wait_for_render();

    assert_eq!(overlay.borrow().inputs, keys);
    assert_eq!(h.tui.viewport_top(), top_before);

    handle.hide();
    wait_for_render();
    h.send("\x1b[5~");
    wait_for_render();
    assert!(h.tui.viewport_top() < top_before);
    h.stop();
}

#[test]
fn keeps_viewport_scrolling_when_an_overlay_is_not_focused() {
    let h = Harness::new(20, 6);
    let editor = input_overlay();
    h.tui.add_child(text(&numbered_lines(12)));
    h.tui.set_focus(Some(editor));
    h.tui.start();
    wait_for_render();
    let top_before = h.tui.viewport_top();

    let hidden = h.tui.show_overlay(input_overlay(), OverlayOptions::default());
    hidden.set_hidden(true);
    let non_capturing = input_overlay();
    h.tui.show_overlay(non_capturing.clone(), OverlayOptions { non_capturing: true, ..OverlayOptions::default() });
    let unfocused = input_overlay();
    let unfocused_handle = h.tui.show_overlay(unfocused.clone(), OverlayOptions::default());
    unfocused_handle.unfocus(None);
    wait_for_render();
    assert!(!non_capturing.borrow().focused);
    assert!(!unfocused.borrow().focused);

    h.send("\x1b[5~");
    h.send("\x1b[<64;10;3M");
    wait_for_render();
    assert!(h.tui.viewport_top() < top_before);
    assert!(non_capturing.borrow().inputs.is_empty());
    assert!(unfocused.borrow().inputs.is_empty());
    h.stop();
}

#[test]
fn keeps_viewport_scrolling_while_transcript_search_is_focused() {
    let h = Harness::new(20, 6);
    h.tui.add_child(text(&numbered_lines(12)));
    h.tui.start();
    wait_for_render();
    let top_before = h.tui.viewport_top();

    h.send("\x1b[102;6u");
    wait_for_render();
    assert!(h.viewport().iter().any(|line| line.contains("↑ ↓")));

    h.send("\x1b[5~");
    h.send("\x1b[<64;1;4M");
    wait_for_render();
    assert!(h.tui.viewport_top() < top_before);
    assert!(h.viewport().iter().any(|line| line.contains("↑ ↓")));
    h.stop();
}
