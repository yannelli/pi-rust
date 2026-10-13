//! Port of upstream `layout.test.ts` ("viewport layout"): scroll views and scrollbars.

mod layout_common;
mod support;

use std::rc::Rc;
use std::time::Duration;

use layout_common::{entry, fn_component, frame, numbered, stripped, text, visible_lines};
use pi_tui::components::{ScrollFollow, StyleFn};
use pi_tui::layout_node::StackAlign;
use pi_tui::terminal_image::{KittyEncodeOptions, KittyImageMetadata, encode_kitty, register_kitty_image_metadata};
use pi_tui::utils::strip_terminal_sequences;
use pi_tui::{
    ComponentRef, HStack, ScrollView, ScrollViewOptions, ScrollViewScrollbar, StackChild, StackOptions, VStack,
    component,
};
use support::Text;

fn last_index_of(line: &str, needle: &str) -> i64 {
    line.rfind(needle).map_or(-1, |index| index as i64)
}

/// JavaScript `lastIndexOf(needle, from)`: the last match starting at or before `from`.
fn last_index_of_from(line: &str, needle: &str, from: i64) -> i64 {
    if from < 0 {
        return -1;
    }
    let mut end = (from as usize + needle.len()).min(line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].rfind(needle).map_or(-1, |index| index as i64)
}

fn has_scrollbar_glyph(lines: &[String]) -> bool {
    lines.iter().any(|line| strip_terminal_sequences(line).contains(['│', '┃']))
}

#[test]
fn crops_kitty_images_at_a_scroll_views_lower_boundary() {
    let _guard = pi_tui::env::global_state_lock();
    let image_id = 124;
    let image_line = encode_kitty(
        "AAAA",
        &KittyEncodeOptions { columns: Some(2), rows: Some(3), image_id: Some(image_id), move_cursor: Some(false) },
    );
    register_kitty_image_metadata(KittyImageMetadata { image_id, columns: 2, rows: 3, width_px: 100, height_px: 100 });
    let content =
        fn_component(move |_| vec!["one".into(), "two".into(), image_line.clone(), String::new(), String::new()]);
    let transcript: ComponentRef = component(ScrollView::new(content, ScrollViewOptions::default()));
    let root: ComponentRef = component(VStack::new(
        vec![entry(transcript).basis(0).grow(1).into(), StackChild::Component(text("dock"))],
        StackOptions::default(),
    ));
    let frame = frame(&root, 20, 4);

    assert!(frame.lines[2].contains("y=0,h=34,r=1"), "{:?}", frame.lines[2]);
}

#[test]
fn tracks_follow_end_state_and_returns_unused_scroll_delta() {
    let options = ScrollViewOptions { follow: ScrollFollow::End, primary: true, ..ScrollViewOptions::default() };
    let scroll_view = ScrollView::new(text("1\n2\n3\n4\n5\n6"), options);
    let state = scroll_view.handle();
    let root: ComponentRef = component(scroll_view);
    frame(&root, 10, 3);
    assert_eq!(state.scroll_top(), 3);
    assert!(state.is_following_end());

    assert_eq!(state.scroll_by(-2), 0);
    assert_eq!(state.scroll_top(), 1);
    assert!(!state.is_following_end());
    assert_eq!(state.scroll_by(-3), -2);
    assert_eq!(state.scroll_top(), 0);
    assert_eq!(state.scroll_by(10), 7);
    assert_eq!(state.scroll_top(), 3);
    assert!(state.is_following_end());
}

#[test]
fn renders_a_proportional_glyph_scrollbar_with_an_expanded_active_thumb() {
    let source_lines = ["abcd界", "abcde2", "abcde3", "abcde4", "abcde5", "abcde6", "abcde7", "abcde8"];
    let content_background = "\x1b[42m";
    let track_color = "\x1b[38;5;2m";
    let thumb_color = "\x1b[38;5;1m";
    let track_style: StyleFn = Rc::new(move |text| format!("{track_color}{text}\x1b[39m"));
    let thumb_style: StyleFn = Rc::new(move |text| format!("{thumb_color}{text}\x1b[39m"));
    let content: ComponentRef = component(
        Text::new(&source_lines.join("\n"), 0, 0).with_bg(move |text| format!("{content_background}{text}\x1b[49m")),
    );
    let styled = |scrollbar, follow| ScrollViewOptions {
        scrollbar,
        follow,
        scrollbar_track_style: Some(track_style.clone()),
        scrollbar_thumb_style: Some(thumb_style.clone()),
        ..ScrollViewOptions::default()
    };
    let scroll_view = ScrollView::new(
        content.clone(),
        ScrollViewOptions {
            scrollbar_hide_delay_ms: Some(10),
            ..styled(ScrollViewScrollbar::Auto, ScrollFollow::None)
        },
    );
    let state = scroll_view.handle();
    let root: ComponentRef = component(scroll_view);
    let render = || frame(&root, 6, 4).lines;
    let expected = |lines: &[&str]| lines.iter().map(|line| line.to_string()).collect::<Vec<_>>();

    let mut lines = render();
    assert_eq!(stripped(&lines), expected(&source_lines[0..4]));

    state.scroll_by(2);
    lines = render();
    assert_eq!(stripped(&lines), expected(&["abcde│", "abcde┃", "abcde┃", "abcde│"]));
    assert_eq!(lines.iter().map(|line| line.contains(track_color)).collect::<Vec<_>>(), [true, false, false, true]);
    assert_eq!(lines.iter().map(|line| line.contains(thumb_color)).collect::<Vec<_>>(), [false, true, true, false]);

    state.set_scrollbar_active(true);
    lines = render();
    assert_eq!(stripped(&lines), expected(&["abcde│", "abcde█", "abcde█", "abcde│"]));
    assert_eq!(lines.iter().map(|line| line.contains(thumb_color)).collect::<Vec<_>>(), [false, true, true, false]);
    assert!(last_index_of(&lines[1], content_background) < last_index_of(&lines[1], thumb_color));

    state.set_scrollbar_active(false);
    pi_tui::ui_loop::run_for(Duration::from_millis(30)).expect("loop");
    lines = render();
    assert_eq!(stripped(&lines), expected(&source_lines[2..6]));

    state.scroll_to_end();
    lines = render();
    assert_eq!(stripped(&lines), expected(&["abcde│", "abcde│", "abcde┃", "abcde┃"]));

    state.scroll_to_start();
    lines = render();
    assert_eq!(stripped(&lines)[0], "abcd ┃");

    let followed_content = component(Text::new(&source_lines.join("\n"), 0, 0));
    let followed = ScrollView::new(followed_content.clone(), styled(ScrollViewScrollbar::Auto, ScrollFollow::End));
    let followed_state = followed.handle();
    let followed: ComponentRef = component(followed);
    frame(&followed, 6, 4);
    assert_eq!(followed_state.scroll_top(), 4);
    followed_content.borrow_mut().set_text(&format!("{}\nabcde9", source_lines.join("\n")));
    let growth_frame = frame(&followed, 6, 4);
    assert_eq!(followed_state.scroll_top(), 5);
    assert!(!has_scrollbar_glyph(&growth_frame.lines));

    let thumb_only = |scrollbar| ScrollViewOptions {
        scrollbar,
        scrollbar_thumb_style: Some(thumb_style.clone()),
        ..Default::default()
    };
    let fitting_content: ComponentRef = component(Text::new("1\n2", 0, 0));
    let automatic = ScrollView::new(fitting_content.clone(), thumb_only(ScrollViewScrollbar::Auto));
    let automatic_state = automatic.handle();
    let automatic: ComponentRef = component(automatic);
    frame(&automatic, 6, 4);
    automatic_state.scroll_by(1);
    assert!(!has_scrollbar_glyph(&frame(&automatic, 6, 4).lines));

    let always_fitting: ComponentRef =
        component(ScrollView::new(fitting_content, thumb_only(ScrollViewScrollbar::Always)));
    let always_fitting_frame = frame(&always_fitting, 6, 4);
    assert_eq!(always_fitting_frame.root.children[0].rect.width, 5);
    assert!(stripped(&always_fitting_frame.lines).iter().all(|line| line.ends_with('┃')));

    let always_overflowing: ComponentRef =
        component(ScrollView::new(content, styled(ScrollViewScrollbar::Always, ScrollFollow::None)));
    let always_overflowing_frame = frame(&always_overflowing, 6, 4);
    assert_eq!(always_overflowing_frame.root.children[0].rect.width, 5);
    let visible = stripped(&always_overflowing_frame.lines);
    assert_eq!(visible.iter().filter(|line| line.ends_with('┃')).count(), 2);
    assert_eq!(visible.iter().filter(|line| line.ends_with('│')).count(), 2);
    for line in &always_overflowing_frame.lines {
        let scrollbar_style_index = last_index_of(line, track_color).max(last_index_of(line, thumb_color));
        let reserved_column_reset_index = last_index_of_from(line, "\x1b[0m\x1b]8;;\x07", scrollbar_style_index);
        assert!(reserved_column_reset_index > last_index_of(line, content_background), "{line:?}");
    }

    let thumb_height_for = |content_height: usize| {
        let sized = ScrollView::new(
            component(Text::new(&vec!["x"; content_height].join("\n"), 0, 0)),
            thumb_only(ScrollViewScrollbar::Auto),
        );
        let sized_state = sized.handle();
        let sized: ComponentRef = component(sized);
        frame(&sized, 6, 20);
        sized_state.scroll_by(1);
        frame(&sized, 6, 20).lines.iter().filter(|line| strip_terminal_sequences(line).ends_with('┃')).count()
    };
    assert_eq!(thumb_height_for(21), 19);
    assert_eq!(thumb_height_for(40), 10);
    assert_eq!(thumb_height_for(100), 4);
    assert_eq!(thumb_height_for(400), 2);
}

#[test]
fn preserves_only_the_underlying_background_beneath_overlay_scrollbar_glyphs() {
    let background = "\x1b[42m";
    let border_foreground = "\x1b[31m";
    let content = fn_component(move |width| {
        vec![format!("{background}{}{border_foreground}│\x1b[39m\x1b[49m", "x".repeat(width - 1)); 8]
    });
    let identity: StyleFn = Rc::new(|text| text.to_string());
    let scroll_view = ScrollView::new(
        content,
        ScrollViewOptions {
            scrollbar: ScrollViewScrollbar::Auto,
            scrollbar_track_style: Some(identity.clone()),
            scrollbar_thumb_style: Some(identity),
            ..ScrollViewOptions::default()
        },
    );
    let state = scroll_view.handle();
    let root: ComponentRef = component(scroll_view);
    frame(&root, 6, 4);
    state.scroll_by(1);
    let frame = frame(&root, 6, 4);

    assert_eq!(stripped(&frame.lines), vec!["xxxxx│", "xxxxx┃", "xxxxx┃", "xxxxx│"]);
    for line in &frame.lines {
        assert!(line.contains(background));
        assert!(!line.contains(border_foreground));
        assert!(line.contains(&format!("\x1b[0m\x1b]8;;\x07{background}")));
    }
}

#[test]
fn updates_reserved_scrollbar_layout_at_runtime() {
    let scroll_view = ScrollView::new(
        text("123456"),
        ScrollViewOptions { scrollbar: ScrollViewScrollbar::Always, ..ScrollViewOptions::default() },
    );
    let state = scroll_view.handle();
    let scroll_view: ComponentRef = component(scroll_view);
    let render = || {
        let root: ComponentRef = component(HStack::new(
            vec![StackChild::Component(scroll_view.clone())],
            StackOptions { align: StackAlign::Start, ..StackOptions::default() },
        ));
        frame(&root, 6, 2)
    };
    let always = render();
    assert_eq!(visible_lines(&always.lines), vec!["12345┃", "6    ┃"]);
    assert_eq!(always.root.children[0].rect.width, 6);
    assert_eq!(always.root.children[0].children[0].rect.width, 5);

    state.set_scrollbar(ScrollViewScrollbar::Hidden);
    assert_eq!(render().root.children[0].children[0].rect.width, 6);
    assert!(!state.is_scrollbar_visible());
}

#[test]
fn measures_nested_scroll_content_from_constrained_child_geometry() {
    let inner = ScrollView::new(text(&numbered(6, "")), ScrollViewOptions::default());
    let inner_state = inner.handle();
    let stack: ComponentRef = component(VStack::new(
        vec![entry(component(inner)).basis(2).into(), StackChild::Component(text("tail"))],
        StackOptions::default(),
    ));
    let outer = ScrollView::new(stack, ScrollViewOptions::default());
    let outer_state = outer.handle();
    let outer: ComponentRef = component(outer);
    frame(&outer, 10, 2);

    assert_eq!(inner_state.viewport_height(), 2);
    assert_eq!(outer_state.scroll_by(10), 9);
    assert_eq!(outer_state.scroll_top(), 1);
}
