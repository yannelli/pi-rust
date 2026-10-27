//! Port of the Kitty and iTerm2 image cases of upstream `test/tui-alt-screen.test.ts`.

mod alt_select_common;
mod support;

use std::cell::RefCell;
use std::rc::Rc;

use alt_select_common::{FnComponent, Harness, TerminalEvent, wait_for_render};
use pi_tui::components::BoxComponent;
use pi_tui::components::scroll_view::StyleFn;
use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::layout_node::StackBasis;
use pi_tui::terminal_image::{
    ImageDimensions, ImageProtocol, KittyEncodeOptions, KittyImageMetadata, TerminalCapabilities, encode_kitty,
    register_kitty_image_metadata, reset_capabilities_cache, set_capabilities,
};
use pi_tui::{
    ComponentRef, HStack, ScrollView, ScrollViewOptions, StackChild, StackEntryOptions, StackOptions,
    TuiAltScreenOptions, TuiOptions, VStack, component,
};
use support::{Image, ImageOptions, ImageTheme, Text};

/// Resets the capability cache when dropped, like the upstream `finally` blocks.
struct ResetCapabilities;

impl Drop for ResetCapabilities {
    fn drop(&mut self) {
        reset_capabilities_cache();
    }
}

fn set_image_capabilities(images: ImageProtocol) -> ResetCapabilities {
    set_capabilities(TerminalCapabilities { images: Some(images), true_color: true, hyperlinks: true });
    ResetCapabilities
}

fn kitty_line(image_id: u32, rows: usize, width_px: u32, height_px: u32) -> String {
    register_kitty_image_metadata(KittyImageMetadata { image_id, columns: 2, rows, width_px, height_px });
    let options =
        KittyEncodeOptions { columns: Some(2), rows: Some(rows), image_id: Some(image_id), move_cursor: Some(false) };
    encode_kitty("AAAA", &options)
}

fn primary_scroll_view(lines: Vec<String>) -> ComponentRef {
    let content: ComponentRef = component(FnComponent::lines(move |_| lines.clone()));
    component(ScrollView::new(content, ScrollViewOptions { primary: true, ..ScrollViewOptions::default() }))
}

#[test]
fn clears_stale_iterm2_image_placements_when_they_leave_the_viewport() {
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Iterm2);
    let h = Harness::new(20, 3);
    let image_line = "\x1b]1337;File=inline=1;width=2;height=auto:AAAA\x07";
    h.tui.add_child(component(FnComponent::fixed(&[image_line, "", "", "after", "more", "end"])));
    h.tui.start();
    wait_for_render();
    h.tui.scroll_to_top();
    wait_for_render();
    let event_count = h.event_count();

    h.tui.scroll_by(1);
    wait_for_render();
    assert!(h.writes_since(event_count).contains("\x1b[2J"));
    h.stop();
}

#[test]
fn crops_a_kitty_image_whose_first_line_is_above_the_viewport() {
    let _guard = global_state_lock();
    let h = Harness::new(20, 3);
    let image_line = kitty_line(123, 3, 100, 100);
    h.tui.add_child(component(FnComponent::fixed(&["before", &image_line, "", "", "after", "end"])));
    h.tui.start();
    wait_for_render();

    assert_eq!(h.tui.viewport_top(), 3);
    assert!(h.events().iter().any(
        |event| matches!(event, TerminalEvent::Write(data) if data.contains("i=123") && data.contains("y=66,h=34,r=1"))
    ));
    h.stop();
}

#[test]
fn crops_a_kitty_image_in_a_padded_colored_box_whose_first_line_is_above_the_viewport() {
    let _guard = global_state_lock();
    let h = Harness::new(20, 3);
    let image_line = kitty_line(11271, 3, 100, 100);
    let bg: StyleFn = Rc::new(|text| format!("\x1b[44m{text}\x1b[49m"));
    let mut boxed = BoxComponent::new(1, 0, Some(bg));
    boxed.add_child(component(FnComponent::fixed(&[&image_line, "", ""])));
    h.tui.add_child(component(FnComponent::fixed(&["before"])));
    h.tui.add_child(component(boxed));
    h.tui.add_child(component(FnComponent::fixed(&["after", "end"])));
    h.tui.start();
    wait_for_render();

    assert_eq!(h.tui.viewport_top(), 3);
    assert!(h.events().iter().any(
        |event| matches!(event, TerminalEvent::Write(data) if data.contains("i=11271") && data.contains("y=66,h=34,r=1"))
    ));
    h.stop();
}

#[test]
fn redraws_wezterm_kitty_images_after_writes_to_covered_rows() {
    // Regression test for #10319: a scrollbar update below an unchanged image anchor erased its cells.
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Kitty);
    let env = MapEnv::new([("WEZTERM_PANE", "1")]);
    let options = TuiOptions { env: Some(Rc::new(env)), ..TuiOptions::default() };
    let h = Harness::with_options(20, 4, options, TuiAltScreenOptions::default());
    let image_line = kitty_line(10319, 3, 100, 100);
    let covered_line = Rc::new(RefCell::new(String::new()));
    let covered = covered_line.clone();
    let root: ComponentRef = component(FnComponent::lines(move |_| {
        vec![image_line.clone(), covered.borrow().clone(), String::new(), "after".to_string()]
    }));
    h.tui.set_layout_root(Some(root));
    h.tui.start();
    wait_for_render();
    let event_count = h.event_count();

    *covered_line.borrow_mut() = "changed".to_string();
    h.tui.request_render(false);
    wait_for_render();
    let redraw_writes = h.writes_since(event_count);
    let placement_index = redraw_writes.find("\x1b_Ga=p,q=2").expect("placement");
    assert!(redraw_writes.contains("\x1b_Ga=d,d=a,q=2\x1b\\"));
    assert!(placement_index > redraw_writes.find("changed").expect("changed row"));
    assert!(!redraw_writes.contains("\x1b_Ga=T"));
    h.stop();
}

#[test]
fn reuses_moved_kitty_images_without_dropping_hstack_siblings() {
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Kitty);
    let h = Harness::new(20, 6);
    let label = component(Text::new("left", 0, 0));
    let dimensions = ImageDimensions { width_px: 100, height_px: 100 };
    let image = component(Image::new(
        &"A".repeat(8192),
        "image/png",
        ImageTheme::default(),
        ImageOptions::default(),
        Some(dimensions),
    ));
    let header = component(Text::new("header", 0, 0));
    let fixed =
        |cells: usize| StackEntryOptions { basis: Some(StackBasis::Fixed(cells)), ..StackEntryOptions::default() };
    let row = component(HStack::new(
        vec![StackChild::Entry(label.clone(), fixed(10)), StackChild::Entry(image, fixed(10))],
        StackOptions::default(),
    ));
    let auto = StackEntryOptions { basis: Some(StackBasis::Auto), ..StackEntryOptions::default() };
    let root: ComponentRef = component(VStack::new(
        vec![StackChild::Entry(header.clone(), auto), StackChild::Entry(row, fixed(4))],
        StackOptions::default(),
    ));
    h.tui.set_layout_root(Some(root));
    h.tui.start();
    wait_for_render();
    assert!(h.any_write_contains("\x1b_Ga=T"));

    let event_count = h.event_count();
    label.borrow_mut().set_text("changed");
    header.borrow_mut().set_text("header\nsecond");
    h.tui.request_render(false);
    wait_for_render();
    let redraw_writes = h.writes_since(event_count);
    let placement_index = redraw_writes.find("\x1b_Ga=p,q=2").expect("placement");
    assert!(redraw_writes.contains("\x1b_Ga=d,d=a,q=2\x1b\\"));
    assert!(placement_index > redraw_writes.find("changed").expect("changed label"));
    assert!(!redraw_writes.contains("\x1b_Ga=T"));
    assert!(redraw_writes.len() < 2000, "expected placement-only redraw, got {} bytes", redraw_writes.len());
    assert!(h.viewport().iter().any(|line| line.trim_end() == "changed"));
    h.stop();
}

#[test]
fn retains_recently_offscreen_kitty_images_for_placement_only_reuse() {
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Kitty);
    let h = Harness::new(20, 1);
    let image_id = 321;
    let image_line = kitty_line(image_id, 1, 100, 50);
    h.tui.set_layout_root(Some(primary_scroll_view(vec![image_line, "after".to_string()])));
    h.tui.start();
    wait_for_render();
    assert!(h.any_write_contains("\x1b_Ga=T"));

    let event_count = h.event_count();
    h.tui.scroll_by(1);
    wait_for_render();
    h.tui.scroll_by(-1);
    wait_for_render();
    let reentry_writes = h.writes_since(event_count);
    assert!(reentry_writes.contains("\x1b_Ga=p,q=2"));
    assert!(!reentry_writes.contains("\x1b_Ga=T"));
    assert!(!reentry_writes.contains(&format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")));
    h.stop();
}

#[test]
fn evicts_the_least_recently_visible_kitty_image_when_the_cache_is_full() {
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Kitty);
    let h = Harness::new(20, 1);
    let first_image_id = 500;
    let image_lines: Vec<String> = (0..18).map(|index| kitty_line(first_image_id + index, 1, 100, 50)).collect();
    let count = image_lines.len();
    h.tui.set_layout_root(Some(primary_scroll_view(image_lines)));
    h.tui.start();
    wait_for_render();
    for _ in 1..count {
        h.tui.scroll_by(1);
        wait_for_render();
    }
    assert!(h.any_write_contains(&format!("\x1b_Ga=d,d=I,i={first_image_id},q=2\x1b\\")));

    let event_count = h.event_count();
    h.tui.scroll_to_top();
    wait_for_render();
    assert!(h.writes_since(event_count).contains("\x1b_Ga=T"));
    h.stop();
}

#[test]
fn evicts_offscreen_kitty_images_when_decoded_raster_memory_exceeds_the_cache_quota() {
    let _guard = global_state_lock();
    let _reset = set_image_capabilities(ImageProtocol::Kitty);
    let h = Harness::new(20, 1);
    let first_image_id = 600;
    let image_lines: Vec<String> = (0..4).map(|index| kitty_line(first_image_id + index, 1, 3840, 2160)).collect();
    let count = image_lines.len();
    h.tui.set_layout_root(Some(primary_scroll_view(image_lines)));
    h.tui.start();
    wait_for_render();
    for _ in 1..count {
        h.tui.scroll_by(1);
        wait_for_render();
    }
    assert!(h.any_write_contains(&format!("\x1b_Ga=d,d=I,i={first_image_id},q=2\x1b\\")));
    h.stop();
}
