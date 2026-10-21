//! Entering and leaving the alternate screen, and the fullscreen render pass.

use std::rc::Rc;

use super::{SelectionGranularity, compose, images, scrollbar, selection, viewport, with_alt};
use crate::component::{CURSOR_MARKER, ComponentRef};
use crate::layout::{OSC133_ZONE_PREFIX, render_layout_frame};
use crate::terminal_image::{
    ImageProtocol, delete_all_kitty_images, delete_all_kitty_placements, get_capabilities,
    get_kitty_image_placement_rows, is_image_line, set_capabilities,
};
use crate::tui::{
    Screen, TuiError, TuiInner, TuiStopOptions, extract_cursor_position, normalize_line_resets, resolve_fake_cursors,
};
use crate::utils::{slice_by_column, visible_width};

const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const BEGIN_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026h";
const END_SYNCHRONIZED_OUTPUT: &str = "\x1b[?2026l";

fn kitty_deletion(protocol: Option<ImageProtocol>) -> String {
    if protocol == Some(ImageProtocol::Kitty) { delete_all_kitty_images() } else { String::new() }
}

pub(crate) fn before_terminal_start(inner: &TuiInner) {
    selection::stop_selection_auto_scroll(inner);
    scrollbar::stop_scrollbar_hover(inner);
    let capabilities = get_capabilities();
    let invalidate = with_alt(inner, |alt| {
        alt.selection_press_active = false;
        alt.scrollbar_drag = None;
        alt.flashes.dispose();
        alt.alt_screen_active = true;
        alt.image_protocol = capabilities.images;
        alt.uploaded_kitty_images.clear();
        let iterm2 = capabilities.images == Some(ImageProtocol::Iterm2);
        if iterm2 {
            alt.saved_capabilities = Some(capabilities);
        }
        alt.last_document.clear();
        alt.selection_anchor = None;
        alt.selection_focus = None;
        alt.selection_granularity = SelectionGranularity::Character;
        alt.selection_initial_range = None;
        alt.last_click = None;
        alt.pressed_url = None;
        alt.selection_dragged = false;
        alt.clear_component_mouse_gesture();
        alt.last_component_click = None;
        alt.reset_render_state();
        iterm2
    })
    .unwrap_or(false);
    if invalidate {
        // iTerm2 images cannot be placed in the alternate screen, so render them as text.
        set_capabilities(crate::terminal_image::TerminalCapabilities { images: None, ..capabilities });
        inner.invalidate_all();
    }
    let term = inner.env.var("TERM").unwrap_or_default().to_lowercase();
    // Multiplexers lag when every pointer movement is forwarded; button-motion tracking still
    // covers clicks, wheel, selections, and scrollbar drags.
    let multiplexer = inner.env.var("TMUX").is_some()
        || inner.env.var("ZELLIJ").is_some()
        || inner.env.var("STY").is_some()
        || term.starts_with("tmux")
        || term.starts_with("screen");
    let mouse_enabled = with_alt(inner, |alt| alt.mouse_enabled).unwrap_or(false);
    let mouse_sequence = match (mouse_enabled, multiplexer) {
        (false, _) => "",
        (true, true) => ENABLE_BUTTON_MOTION_MOUSE,
        (true, false) => ENABLE_ALL_MOTION_MOUSE,
    };
    inner.write(&format!("{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse_sequence}\x1b[2J\x1b[H\x1b[?25l"));
}

/// `(alternate screen active, mouse tracking on)` for the exit fallback, which must not panic on a
/// borrowed state.
fn exit_state(inner: &TuiInner) -> Option<(bool, bool)> {
    match &*inner.screen.try_borrow().ok()? {
        Screen::Alt(alt) => Some((alt.alt_screen_active, alt.mouse_enabled)),
        Screen::Main(_) => None,
    }
}

/// Resets written before the base ones when the TUI goes away without `stop`: end synchronized
/// output, mouse tracking, and autowrap, while the alternate screen is active.
pub(crate) fn exit_restore_prefix(inner: &TuiInner) -> String {
    match exit_state(inner) {
        Some((true, mouse)) => {
            let mouse = if mouse { DISABLE_MOUSE } else { "" };
            format!("{END_SYNCHRONIZED_OUTPUT}{mouse}{ENABLE_AUTOWRAP}")
        }
        _ => String::new(),
    }
}

/// Resets written after the terminal's own. Kitty keyboard flags are per screen, so the terminal
/// pops them before the alternate screen is left.
pub(crate) fn exit_restore_suffix(inner: &TuiInner) -> String {
    match exit_state(inner) {
        Some((true, _)) => format!("{EXIT_ALT_SCREEN}\x1b[?25h"),
        _ => String::new(),
    }
}

pub(crate) fn before_terminal_stop(inner: &TuiInner, _options: TuiStopOptions) {
    viewport::close_search(inner);
    selection::stop_selection_auto_scroll(inner);
    scrollbar::stop_scrollbar_hover(inner);
    let output = with_alt(inner, |alt| {
        alt.selection_press_active = false;
        alt.scrollbar_drag = None;
        alt.clear_component_mouse_gesture();
        alt.flashes.dispose();
        if !alt.alt_screen_active {
            return None;
        }
        let mouse = if alt.mouse_enabled { DISABLE_MOUSE } else { "" };
        let deletion = kitty_deletion(alt.image_protocol);
        alt.uploaded_kitty_images.clear();
        Some(format!("{BEGIN_SYNCHRONIZED_OUTPUT}{deletion}{mouse}{ENABLE_AUTOWRAP}{END_SYNCHRONIZED_OUTPUT}"))
    })
    .flatten();
    if let Some(output) = output {
        inner.write(&output);
    }
}

pub(crate) fn after_terminal_stop(inner: &TuiInner, options: TuiStopOptions) {
    let active = with_alt(inner, |alt| std::mem::replace(&mut alt.alt_screen_active, false)).unwrap_or(false);
    if !active {
        return;
    }
    if options.preserve_screen {
        inner.write(&format!("{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"));
    } else {
        let width = inner.columns().max(1);
        let show_hardware_cursor = inner.state.borrow().show_hardware_cursor;
        let lines = resolve_fake_cursors(inner.render_root(width), show_hardware_cursor);
        let mut document: Vec<String> =
            lines.iter().map(|line| OSC133_ZONE_PREFIX.replace(line, "").replace(CURSOR_MARKER, "")).collect();
        normalize_line_resets(&mut document);
        let document: Vec<String> = document
            .into_iter()
            .map(|line| {
                if is_image_line(&line) || visible_width(&line) <= width {
                    line
                } else {
                    slice_by_column(&line, 0, width, true)
                }
            })
            .collect();
        let mut buffer = format!("{BEGIN_SYNCHRONIZED_OUTPUT}{EXIT_ALT_SCREEN}{DISABLE_AUTOWRAP}");
        for (row, line) in document.iter().enumerate() {
            if row > 0 {
                buffer.push_str("\r\n");
            }
            buffer.push_str("\r\x1b[2K");
            buffer.push_str(line);
        }
        buffer.push_str(&format!("\x1b[0m{ENABLE_AUTOWRAP}\r\n\x1b[?25h{END_SYNCHRONIZED_OUTPUT}"));
        with_alt(inner, |alt| alt.last_document = document);
        inner.write(&buffer);
    }
    if let Some(saved) = with_alt(inner, |alt| alt.saved_capabilities.take()).flatten() {
        set_capabilities(saved);
    }
}

pub(crate) fn do_render(inner: &TuiInner) -> Result<(), TuiError> {
    let active = with_alt(inner, |alt| alt.alt_screen_active).unwrap_or(false);
    if inner.is_stopped() || !active {
        return Ok(());
    }
    let width = inner.columns().max(1);
    let height = inner.rows().max(1);
    let root: ComponentRef = inner
        .layout_root
        .borrow()
        .clone()
        .or_else(|| with_alt(inner, |alt| alt.implicit_scroll_view.clone()))
        .expect("fullscreen state");
    let weak = inner.weak.clone();
    let request_render: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(tui) = weak.upgrade() {
            tui.request_render(false);
        }
    });
    let mut layout = render_layout_frame(&root, width, height, request_render.clone());
    if viewport::refresh_search(inner, &layout) {
        layout = render_layout_frame(&root, width, height, request_render);
    }
    let show_hardware_cursor = inner.state.borrow().show_hardware_cursor;
    let stripped = layout.lines.iter().map(|line| OSC133_ZONE_PREFIX.replace(line, "").into_owned()).collect();
    let mut screen = resolve_fake_cursors(stripped, show_hardware_cursor);
    screen = compose::apply_search_highlights(inner, screen, &layout);
    screen = compose::composite_scroll_to_end_indicator(inner, screen, &layout, width);
    screen = inner.composite_overlays(screen, width, height);
    if screen.len() > height {
        screen.drain(..screen.len() - height);
    }
    screen = compose::apply_selection(inner, screen, Some(&layout));
    screen = compose::composite_flashes(inner, screen, width, height);
    let cursor_pos = extract_cursor_position(&mut screen, height);
    normalize_line_resets(&mut screen);
    let screen: Vec<String> = screen
        .into_iter()
        .map(|line| {
            if is_image_line(&line) || visible_width(&line) <= width {
                line
            } else {
                slice_by_column(&line, 0, width, true)
            }
        })
        .collect();
    let wezterm = inner.env.non_empty("WEZTERM_PANE").is_some()
        || inner.env.var("TERM_PROGRAM").is_some_and(|p| p.eq_ignore_ascii_case("wezterm"));

    let output = with_alt(inner, |alt| {
        let full_redraw = alt.previous_screen.is_empty()
            || alt.previous_screen_width != width
            || alt.previous_screen_height != height;
        let previous = |row: usize| alt.previous_screen.get(row).map(String::as_str).unwrap_or("");
        let changed: Vec<bool> = screen.iter().enumerate().map(|(row, line)| line != previous(row)).collect();
        let anchors_redraw = screen
            .iter()
            .enumerate()
            .any(|(row, line)| changed[row] && (is_image_line(line) || is_image_line(previous(row))));
        let kitty = alt.image_protocol == Some(ImageProtocol::Kitty);
        let cells_redraw = !anchors_redraw
            && wezterm
            && kitty
            && changed.iter().any(|c| *c)
            && screen.iter().enumerate().any(|(row, line)| {
                get_kitty_image_placement_rows(line)
                    .is_some_and(|rows| (row..row + rows).any(|covered| changed.get(covered).copied().unwrap_or(false)))
            });
        let images_redraw = anchors_redraw || cells_redraw;
        let redraw_images = full_redraw || images_redraw;
        let had_uploaded = !alt.uploaded_kitty_images.is_empty();
        let (lines, evicted) = if redraw_images && kitty {
            images::prepare_kitty_screen(alt, &screen)
        } else {
            (screen.clone(), String::new())
        };

        let mut buffer = String::from(BEGIN_SYNCHRONIZED_OUTPUT);
        let redraw_row = |row: usize| full_redraw || images_redraw || changed.get(row).copied().unwrap_or(true);
        if full_redraw {
            let clear =
                if kitty && had_uploaded { delete_all_kitty_placements() } else { kitty_deletion(alt.image_protocol) };
            buffer.push_str(&clear);
            buffer.push_str("\x1b[2J");
        } else if images_redraw {
            match alt.image_protocol {
                Some(ImageProtocol::Iterm2) => buffer.push_str("\x1b[2J"),
                Some(ImageProtocol::Kitty) => buffer.push_str(&delete_all_kitty_placements()),
                None => {}
            }
        }
        buffer.push_str(&evicted);
        let line_at = |row: usize| lines.get(row).map(String::as_str).unwrap_or("");
        // WezTerm erases image cells when a later row write touches a covered row, so draw image
        // placements after every clear and text write.
        if redraw_images && kitty && wezterm && screen.iter().any(|l| is_image_line(l)) {
            for row in (0..height).filter(|&r| redraw_row(r)) {
                buffer.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
            }
            for row in (0..height).filter(|&r| redraw_row(r) && !is_image_line(line_at(r))) {
                buffer.push_str(&format!("\x1b[{};1H{}", row + 1, line_at(row)));
            }
            for row in (0..height).filter(|&r| redraw_row(r) && is_image_line(line_at(r))) {
                buffer.push_str(&format!("\x1b[{};1H{}", row + 1, line_at(row)));
            }
        } else {
            for row in (0..height).filter(|&r| redraw_row(r)) {
                buffer.push_str(&format!("\x1b[{};1H\x1b[2K{}", row + 1, line_at(row)));
            }
        }
        match cursor_pos {
            Some((row, col)) => {
                buffer.push_str(&format!("\x1b[{};{}H", row + 1, col.min(width) + 1));
                buffer.push_str(if show_hardware_cursor { "\x1b[?25h" } else { "\x1b[?25l" });
            }
            None => buffer.push_str("\x1b[?25l"),
        }
        buffer.push_str(END_SYNCHRONIZED_OUTPUT);
        (buffer, full_redraw)
    });
    let Some((buffer, full_redraw)) = output else { return Ok(()) };
    if full_redraw {
        inner.state.borrow_mut().full_redraw_count += 1;
    }
    inner.write(&buffer);
    with_alt(inner, |alt| {
        alt.previous_screen = screen;
        alt.previous_screen_width = width;
        alt.previous_screen_height = height;
        alt.current_layout = Some(layout);
    });
    Ok(())
}
