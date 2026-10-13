//! Port of upstream `layout.test.ts` ("viewport layout"): stack allocation and composition.

mod layout_common;
mod support;

use std::cell::Cell;
use std::rc::Rc;

use layout_common::{entry, fn_component, frame, text, visible_lines};
use pi_tui::{
    Component, ComponentRef, HStack, ScrollView, ScrollViewOptions, StackChild, StackOptions, VStack, component,
};

#[test]
fn allocates_vertical_grow_space_deterministically() {
    let root: ComponentRef = component(VStack::new(
        vec![entry(text("top")).basis(1).shrink(0).into(), entry(text("body")).basis(0).grow(1).into()],
        StackOptions::default(),
    ));
    let frame = frame(&root, 10, 4);

    assert_eq!(frame.root.children.iter().map(|child| child.rect.height).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(visible_lines(&frame.lines), vec!["top", "body", "", ""]);
}

#[test]
fn does_not_render_fixed_basis_scroll_content_during_stack_measurement() {
    let render_count = Rc::new(Cell::new(0));
    let counter = render_count.clone();
    let content = fn_component(move |_| {
        counter.set(counter.get() + 1);
        vec!["one".into(), "two".into(), "three".into()]
    });
    let transcript: ComponentRef = component(ScrollView::new(content, ScrollViewOptions::default()));
    let root: ComponentRef = component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).into(), entry(text("dock")).auto().into()],
        StackOptions::default(),
    ));
    frame(&root, 10, 3);
    assert_eq!(render_count.get(), 1);
}

#[test]
fn paints_only_clipped_rows_from_very_large_scroll_content() {
    // Upstream uses a sparse array of one billion lines; a Vec of that size cannot be allocated.
    let line_count = 1_000_000;
    let mut lines = vec![String::new(); line_count];
    lines[line_count - 4] = "before".into();
    lines[line_count - 3] = "visible 1".into();
    lines[line_count - 2] = "visible 2".into();
    lines[line_count - 1] = "visible 3".into();
    let lines = Rc::new(lines);
    let content = fn_component(move |_| lines.as_ref().clone());
    let options = ScrollViewOptions { follow: pi_tui::components::ScrollFollow::End, ..ScrollViewOptions::default() };
    let transcript: ComponentRef = component(ScrollView::new(content, options));

    let frame = frame(&transcript, 10, 3);
    assert_eq!(visible_lines(&frame.lines), vec!["visible 1", "visible 2", "visible 3"]);
}

#[test]
fn shrinks_entries_to_their_minimum_sizes() {
    let root: ComponentRef = component(VStack::new(
        vec![entry(text("a1\na2\na3")).shrink(1).min(1).into(), entry(text("b1\nb2\nb3")).shrink(0).into()],
        StackOptions::default(),
    ));
    let frame = frame(&root, 10, 4);

    assert_eq!(frame.root.children.iter().map(|child| child.rect.height).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(visible_lines(&frame.lines), vec!["a1", "b1", "b2", "b3"]);
}

#[test]
fn includes_nested_minimum_sizes_in_intrinsic_stack_measurement() {
    let dock: ComponentRef = component(VStack::new(
        vec![
            StackChild::Component(text("top1\ntop2\ntop3")),
            entry(text("selector")).min(3).into(),
            StackChild::Component(text("below")),
            entry(text("footer")).min(1).into(),
        ],
        StackOptions::default(),
    ));
    let root: ComponentRef = component(VStack::new(
        vec![entry(text("body")).basis(0).grow(1).min(1).into(), entry(dock).auto().min(1).into()],
        StackOptions::default(),
    ));
    let frame = frame(&root, 10, 9);

    assert_eq!(
        visible_lines(&frame.lines),
        vec!["body", "top1", "top2", "top3", "selector", "", "", "below", "footer"]
    );
}

#[test]
fn omits_gaps_around_invisible_entries() {
    let mut stack = VStack::new(
        vec![
            StackChild::Component(text("one")),
            entry(text("hidden")).visible(|_| false).into(),
            StackChild::Component(text("two")),
        ],
        StackOptions { gap: 1, ..StackOptions::default() },
    );
    let lines: Vec<String> = stack.render(10).iter().map(|line| line.trim_end().to_string()).collect();
    assert_eq!(lines, vec!["one", "", "two"]);
}

#[test]
fn composes_horizontal_children_at_allocated_widths() {
    let root: ComponentRef = component(HStack::new(
        vec![entry(text("left")).basis(6).shrink(0).into(), entry(text("right")).basis(6).shrink(0).into()],
        StackOptions::default(),
    ));
    let frame = frame(&root, 12, 1);
    assert_eq!(visible_lines(&frame.lines), vec!["left  right"]);
}

#[test]
fn does_not_paint_zero_width_horizontal_children() {
    let root: ComponentRef = component(HStack::new(
        vec![entry(text("hidden")).basis(0).shrink(0).into(), entry(text("shown")).basis(0).grow(1).into()],
        StackOptions::default(),
    ));
    let frame = frame(&root, 5, 1);
    assert_eq!(visible_lines(&frame.lines), vec!["shown"]);
}

#[test]
fn rebuilds_geometry_after_content_changes() {
    let content = text("one");
    let root: ComponentRef =
        component(VStack::new(vec![StackChild::Component(content.clone())], StackOptions::default()));
    let first = frame(&root, 10, 4);
    content.borrow_mut().set_text("one\ntwo\nthree");
    let second = frame(&root, 10, 4);

    assert_eq!(first.root.children[0].lines.as_ref().map(|lines| lines.len()), Some(1));
    assert_eq!(second.root.children[0].lines.as_ref().map(|lines| lines.len()), Some(3));
}
