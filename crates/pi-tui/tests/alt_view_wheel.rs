//! Port of upstream `tui-alt-screen.test.ts`: wheel routing, wheel line counts, and layout
//! mouse dispatch.

mod alt_view_common;
mod layout_common;
mod support;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use alt_view_common::{Harness, numbered_lines, scroll_view};
use layout_common::{entry, text};
use pi_tui::components::ScrollFollow;
use pi_tui::utils::truncate_to_width;
use pi_tui::{
    Component, ComponentRef, HStack, MouseResponse, ScrollViewOptions, StackChild, StackOptions, TuiAltScreenOptions,
    TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType, VStack, WheelScrollLines, component,
};
use support::MouseRegion;

fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

fn wheel_lines(lines: f64) -> TuiAltScreenOptions {
    TuiAltScreenOptions { wheel_scroll_lines: Some(WheelScrollLines::Fixed(lines)), ..Default::default() }
}

#[test]
fn routes_wheel_input_to_the_scroll_view_under_the_pointer() {
    let h = Harness::new(20, 4);
    let left_options = ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..Default::default() };
    let (left, left_state) = scroll_view(text("a1\na2\na3\na4\na5\na6\na7"), left_options);
    let right_options = ScrollViewOptions { follow: ScrollFollow::End, ..Default::default() };
    let (right, right_state) = scroll_view(text("b1\nb2\nb3\nb4\nb5\nb6\nb7"), right_options);
    h.tui.set_layout_root(Some(component(HStack::new(
        vec![entry(left).basis(10).shrink(0).into(), entry(right).basis(10).shrink(0).into()],
        StackOptions::default(),
    ))));
    h.tui.start();
    h.wait();

    h.send("\x1b[<64;15;1M");
    h.wait();
    assert_eq!(left_state.scroll_top(), 3);
    assert_eq!(right_state.scroll_top(), 2);
    assert_eq!(h.viewport(), strings(&["a4        b3", "a5        b4", "a6        b5", "a7        b6"]));
    h.stop();
}

#[test]
fn scrolls_faster_while_alt_is_held_during_wheel_input() {
    let h = Harness::new(20, 4);
    h.tui.add_child(text(&numbered_lines(12)));
    h.tui.start();
    h.wait();
    assert_eq!(h.tui.viewport_top(), 8);

    // Alt modifier sets bit 8 on the wheel button (72 = 64 + 8).
    h.send("\x1b[<72;1;1M");
    h.wait();
    assert_eq!(h.tui.viewport_top(), 3);
    h.stop();
}

/// The parts of upstream `SelectList` this case needs: one row per item and mouse selection.
struct TestSelectList {
    labels: Vec<&'static str>,
    selected: usize,
    pressed: Option<usize>,
    on_select: Box<dyn FnMut(&str)>,
}

impl Component for TestSelectList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let rows = self.labels.iter().enumerate().map(|(index, label)| {
            let prefix = if index == self.selected { "→ " } else { "  " };
            truncate_to_width(&format!("{prefix}{label}"), width)
        });
        rows.collect()
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        let pressed = event.kind == TuiMouseEventType::Press;
        if event.button != TuiMouseButton::Left || !(pressed || event.kind == TuiMouseEventType::Click) {
            return None;
        }
        let index = usize::try_from(event.y).ok().filter(|&index| index < self.labels.len())?;
        if pressed {
            self.pressed = Some(index);
            self.selected = index;
            return Some(MouseResponse::Handled(TuiMouseEventResult {
                handled: true,
                focus: true,
                ..Default::default()
            }));
        }
        self.selected = self.pressed.take().unwrap_or(index);
        (self.on_select)(self.labels[self.selected]);
        Some(MouseResponse::Handled(TuiMouseEventResult::handled()))
    }
}

#[test]
fn does_not_vertically_redispatch_misses_through_horizontal_layout_containers() {
    let h = Harness::new(20, 2);
    let selections = Rc::new(Cell::new(0));
    let counter = selections.clone();
    let list: ComponentRef = component(TestSelectList {
        labels: vec!["A", "B"],
        selected: 0,
        pressed: None,
        on_select: Box::new(move |_| counter.set(counter.get() + 1)),
    });
    h.tui.set_layout_root(Some(component(HStack::new(
        vec![entry(list).basis(10).into(), entry(text("plain")).basis(10).into()],
        StackOptions::default(),
    ))));
    h.tui.start();
    h.wait();

    h.send("\x1b[<0;15;1M");
    h.send("\x1b[<0;15;1m");
    h.wait();
    assert_eq!(selections.get(), 0);
    h.stop();
}

// #9758: wheel line counts can change at runtime; Alt keeps its multiplier.
#[test]
fn applies_runtime_wheel_line_count_updates() {
    let h = Harness::with_options(20, 4, wheel_lines(3.0));
    let deltas: Rc<RefCell<Vec<Option<i32>>>> = Rc::default();
    let recorded = deltas.clone();
    h.tui.add_child(component(MouseRegion::new(text("wheel target"), move |event| {
        if event.kind != TuiMouseEventType::Wheel {
            return None;
        }
        recorded.borrow_mut().push(event.wheel_delta);
        Some(TuiMouseEventResult::handled())
    })));
    h.tui.start();
    h.wait();
    h.send("\x1b[<64;1;1M");
    h.tui.set_wheel_scroll_lines(WheelScrollLines::Fixed(2.0));
    h.send("\x1b[<65;1;1M");
    h.send("\x1b[<72;1;1M");
    assert_eq!(*deltas.borrow(), vec![Some(-3), Some(2), Some(-10)]);
    h.stop();
}

#[test]
fn chains_unused_wheel_delta_to_an_outer_scroll_view() {
    let h = Harness::with_options(20, 4, wheel_lines(3.0));
    let (inner, inner_state) = scroll_view(text("i1\ni2\ni3\ni4\ni5\ni6"), ScrollViewOptions::default());
    let stack: ComponentRef = component(VStack::new(
        vec![entry(inner).basis(2).into(), StackChild::Component(text("tail1\ntail2\ntail3\ntail4\ntail5"))],
        StackOptions::default(),
    ));
    let (outer, outer_state) = scroll_view(stack, ScrollViewOptions { primary: true, ..Default::default() });
    h.tui.set_layout_root(Some(outer));
    h.tui.start();
    h.wait();

    h.send("\x1b[<65;1;1M");
    h.wait();
    assert_eq!(inner_state.scroll_top(), 3);
    assert_eq!(outer_state.scroll_top(), 0);

    h.send("\x1b[<65;1;1M");
    h.wait();
    assert_eq!(inner_state.scroll_top(), 4);
    assert_eq!(outer_state.scroll_top(), 2);
    h.stop();
}
