//! Port of upstream `truncated-text.test.ts`, plus tests of `Text`, `Box`, `Spacer`, `Loader`,
//! `CancellableLoader`, `Image`, and `MouseRegion` against upstream behavior.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use pi_tui::components::{
    BoxComponent, CancellableLoader, Image, ImageOptions, ImageTheme, Loader, LoaderIndicatorOptions, MouseRegion,
    Spacer, StyleFn, Text, TruncatedText,
};
use pi_tui::terminal_image::{
    CapabilityOverrides, ImageDimensions, ImageProtocol, TerminalCapabilities, reset_capabilities_cache,
    set_capabilities,
};
use pi_tui::utils::visible_width;
use pi_tui::{
    Component, ComponentRef, MouseResponse, TuiMouseButton, TuiMouseEvent, TuiMouseEventResult, TuiMouseEventType,
    component, ui_loop,
};

fn strip_sgr(line: &str) -> String {
    regex::Regex::new(r"\x1b\[[0-9;]*m").expect("regex").replace_all(line, "").into_owned()
}

fn red(text: &str) -> String {
    format!("\x1b[31m{text}\x1b[39m")
}

fn blue(text: &str) -> String {
    format!("\x1b[34m{text}\x1b[39m")
}

#[test]
fn truncated_text_pads_output_lines_to_exactly_match_width() {
    let lines = TruncatedText::new("Hello world", 1, 0).render(50);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 50);
}

#[test]
fn truncated_text_pads_output_with_vertical_padding_lines_to_width() {
    let lines = TruncatedText::new("Hello", 0, 2).render(40);
    assert_eq!(lines.len(), 5);
    assert!(lines.iter().all(|line| visible_width(line) == 40));
}

#[test]
fn truncated_text_truncates_long_text_and_pads_to_width() {
    let long = "This is a very long piece of text that will definitely exceed the available width";
    let lines = TruncatedText::new(long, 1, 0).render(30);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);
    assert!(strip_sgr(&lines[0]).contains("..."));
}

#[test]
fn truncated_text_preserves_ansi_codes_in_output_and_pads_correctly() {
    let styled = format!("{} {}", red("Hello"), blue("world"));
    let lines = TruncatedText::new(&styled, 1, 0).render(40);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 40);
    assert!(lines[0].contains("\x1b["));
}

#[test]
fn truncated_text_truncates_styled_text_and_adds_reset_code_before_ellipsis() {
    let styled = red("This is a very long red text that will be truncated");
    let lines = TruncatedText::new(&styled, 1, 0).render(20);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 20);
    assert!(lines[0].contains("\x1b[0m..."));
}

#[test]
fn truncated_text_handles_text_that_fits_exactly() {
    let lines = TruncatedText::new("Hello world", 1, 0).render(30);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);
    assert!(!strip_sgr(&lines[0]).contains("..."));
}

#[test]
fn truncated_text_handles_empty_text() {
    let lines = TruncatedText::new("", 1, 0).render(30);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 30);
}

#[test]
fn truncated_text_stops_at_newline_and_only_shows_first_line() {
    let lines = TruncatedText::new("First line\nSecond line\nThird line", 1, 0).render(40);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 40);
    let stripped = strip_sgr(&lines[0]);
    assert!(stripped.trim().contains("First line"));
    assert!(!stripped.contains("Second line") && !stripped.contains("Third line"));
}

#[test]
fn truncated_text_truncates_first_line_even_with_newlines_in_text() {
    let lines =
        TruncatedText::new("This is a very long first line that needs truncation\nSecond line", 1, 0).render(25);
    assert_eq!(lines.len(), 1);
    assert_eq!(visible_width(&lines[0]), 25);
    let stripped = strip_sgr(&lines[0]);
    assert!(stripped.contains("...") && !stripped.contains("Second line"));
}

#[test]
fn text_wraps_pads_and_applies_background() {
    let mut text = Text::new("alpha beta gamma", 1, 1);
    let lines = text.render(10);
    assert_eq!(lines, ["          ", " alpha    ", " beta     ", " gamma    ", "          "]);
    assert!(Text::new("   ", 1, 1).render(10).is_empty());

    let mut bg = Text::new("a\tb", 0, 0).with_bg(|line| format!("<{line}>"));
    assert_eq!(bg.render(6), ["<a   b >"]);
    // Margins shrink so the content keeps at least one column.
    assert_eq!(Text::new("abc", 5, 0).render(3), [" a ", " b ", " c "]);
}

#[test]
fn text_caches_until_text_or_padding_changes() {
    let mut text = Text::new("one", 0, 0);
    assert_eq!(text.render(5), ["one  "]);
    text.set_text("two");
    assert_eq!(text.render(5), ["two  "]);
    text.set_padding_x(1);
    assert_eq!(text.render(5), [" two "]);
}

#[test]
fn spacer_renders_empty_lines() {
    let mut spacer = Spacer::default();
    assert_eq!(spacer.render(10), [""]);
    spacer.set_lines(3);
    assert_eq!(spacer.render(10), ["", "", ""]);
}

#[test]
fn box_pads_children_and_applies_background() {
    let bg: StyleFn = Rc::new(|line| format!("[{line}]"));
    let mut boxed = BoxComponent::new(2, 1, Some(bg));
    assert!(boxed.render(10).is_empty());
    boxed.add_child(component(Text::new("hi", 0, 0)));
    let lines = boxed.render(10);
    assert_eq!(lines, ["[          ]", "[  hi      ]", "[          ]"]);

    // A new background is detected by sampling, without invalidation.
    boxed.set_bg_fn(Some(Rc::new(|line| format!("{{{line}}}"))));
    assert_eq!(boxed.render(10)[1], "{  hi      }");
}

#[test]
fn box_forwards_mouse_events_to_the_child_under_the_pointer() {
    let hits: Rc<RefCell<Vec<(i32, i32, i32)>>> = Rc::default();
    let sink = hits.clone();
    let region = MouseRegion::new(component(Text::new("a\nb", 0, 0)), move |event| {
        sink.borrow_mut().push((event.x, event.y, event.width));
        Some(TuiMouseEventResult::handled())
    });
    let mut boxed = BoxComponent::new(1, 1, None);
    boxed.add_child(component(Text::new("top", 0, 0)));
    boxed.add_child(component(region));
    boxed.render(12);

    let event = |x, y| TuiMouseEvent {
        kind: TuiMouseEventType::Press,
        button: TuiMouseButton::Left,
        x,
        y,
        screen_x: x,
        screen_y: y,
        width: 12,
        height: 5,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: None,
    };
    assert!(boxed.handle_mouse(&event(3, 3)).is_some_and(|r| matches!(r, MouseResponse::Forwarded(_))));
    assert_eq!(*hits.borrow(), [(2, 1, 10)]);
    assert!(boxed.handle_mouse(&event(0, 2)).is_none(), "padding is not content");
    assert!(boxed.handle_mouse(&event(3, 0)).is_none(), "top padding is not content");
}

fn tagged(tag: &'static str) -> StyleFn {
    Rc::new(move |text| format!("<{tag}>{text}</{tag}>"))
}

#[test]
fn loader_renders_spinner_and_message_and_animates_on_the_ui_loop() {
    let mut loader = Loader::new(None, tagged("s"), tagged("m"), "Working", None);
    let first = loader.render(40);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0], "");
    assert!(first[1].starts_with(" <s>⠋</s> <m>Working</m>"), "{first:?}");

    ui_loop::run_for(Duration::from_millis(200)).expect("loop");
    let later = loader.render(40);
    assert!(!later[1].contains("⠋"), "the spinner advanced: {later:?}");

    loader.set_message("Done");
    assert!(loader.render(40)[1].contains("<m>Done</m>"));
    loader.stop();
    let stopped = loader.render(40);
    ui_loop::run_for(Duration::from_millis(200)).expect("loop");
    assert_eq!(loader.render(40), stopped);
}

#[test]
fn loader_renders_custom_indicators_verbatim() {
    let static_frame = LoaderIndicatorOptions { frames: Some(vec!["*".into()]), interval_ms: None };
    let mut loader = Loader::new(None, tagged("s"), tagged("m"), "Busy", Some(static_frame));
    assert!(loader.render(20)[1].starts_with(" * <m>Busy</m>"));

    loader.set_indicator(Some(LoaderIndicatorOptions { frames: Some(Vec::new()), interval_ms: None }));
    assert!(loader.render(20)[1].starts_with(" <m>Busy</m>"));
}

#[test]
fn cancellable_loader_cancels_on_escape() {
    let aborted = Rc::new(Cell::new(false));
    let flag = aborted.clone();
    let mut loader = CancellableLoader::new(None, tagged("s"), tagged("m"), "Working", None);
    loader.on_abort = Some(Box::new(move || flag.set(true)));
    let signal = loader.signal();
    loader.handle_input("x");
    assert!(!loader.aborted());
    loader.handle_input("\x1b");
    assert!(loader.aborted() && signal.is_cancelled() && aborted.get());
    loader.dispose();
}

fn with_capabilities<T>(images: Option<ImageProtocol>, run: impl FnOnce() -> T) -> T {
    let _guard = pi_tui::env::global_state_lock();
    set_capabilities(TerminalCapabilities { images, true_color: true, hyperlinks: false });
    let result = run();
    pi_tui::terminal_image::set_capability_overrides(CapabilityOverrides::default());
    reset_capabilities_cache();
    result
}

#[test]
fn image_renders_a_styled_fallback_without_image_support() {
    let lines = with_capabilities(None, || {
        let theme = ImageTheme { fallback_color: tagged("f") };
        let options = ImageOptions { filename: Some("cat.png".into()), ..ImageOptions::default() };
        let dimensions = ImageDimensions { width_px: 10, height_px: 20 };
        Image::new("AAAA", "image/png", theme, options, Some(dimensions)).render(80)
    });
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("<f>") && lines[0].contains("cat.png"), "{lines:?}");
}

#[test]
fn image_reserves_rows_for_kitty_and_iterm2_images() {
    let dimensions = ImageDimensions { width_px: 90, height_px: 180 };
    let options = ImageOptions { max_width_cells: Some(10), ..ImageOptions::default() };
    let (kitty, id) = with_capabilities(Some(ImageProtocol::Kitty), || {
        let mut image = Image::new("AAAA", "image/png", ImageTheme::default(), options.clone(), Some(dimensions));
        (image.render(40), image.image_id())
    });
    assert!(kitty[0].starts_with("\x1b_G") && kitty.len() > 1);
    assert!(kitty[1..].iter().all(String::is_empty));
    assert!(id.is_some());

    let iterm = with_capabilities(Some(ImageProtocol::Iterm2), || {
        Image::new("AAAA", "image/png", ImageTheme::default(), options, Some(dimensions)).render(40)
    });
    let last = iterm.last().expect("line");
    assert!(last.starts_with(&format!("\x1b[{}A\x1b]1337;", iterm.len() - 1)), "{iterm:?}");
}

#[test]
fn image_uses_the_transcoder_for_non_png_images_on_kitty() {
    let lines = with_capabilities(Some(ImageProtocol::Kitty), || {
        pi_tui::components::set_image_transcoder(None);
        let jpeg = Image::new("AAAA", "image/jpeg", ImageTheme::default(), ImageOptions::default(), None).render(40);
        pi_tui::components::set_image_transcoder(Some(std::sync::Arc::new(|_data: &str, mime: &str| {
            (mime == "image/jpeg").then(|| "AAAA".to_string())
        })));
        let converted =
            Image::new("BBBB", "image/jpeg", ImageTheme::default(), ImageOptions::default(), None).render(40);
        pi_tui::components::set_image_transcoder(None);
        (jpeg, converted)
    });
    assert!(lines.0[0].contains("image/jpeg"), "no transcoder falls back: {:?}", lines.0);
    assert!(lines.1[0].starts_with("\x1b_G"), "{:?}", lines.1);
}

#[test]
fn mouse_region_prefers_the_child_and_then_its_handler() {
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    let child: ComponentRef = component(Text::new("label", 0, 0));
    let mut region = MouseRegion::new(child, move |_event| {
        counter.set(counter.get() + 1);
        None
    });
    assert_eq!(region.render(8), ["label   "]);
    let event = TuiMouseEvent {
        kind: TuiMouseEventType::Click,
        button: TuiMouseButton::Left,
        x: 0,
        y: 0,
        screen_x: 0,
        screen_y: 0,
        width: 8,
        height: 1,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: Some(1),
    };
    assert!(region.handle_mouse(&event).is_none());
    assert_eq!(calls.get(), 1);
}
