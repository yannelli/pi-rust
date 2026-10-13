//! Mouse parsing and routing to components, overlays, wheel scrolling, and selection.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

use super::scrollbar::{
    handle_scroll_to_end_indicator, handle_scrollbar_mouse_event, handle_search_mouse_event, stop_scrollbar_hover,
    update_scrollbar_hover,
};
use super::{ComponentClick, selection, with_alt};
use crate::component::{
    ComponentRef, TuiMouseButton, TuiMouseDispatchResult, TuiMouseDispatchTarget, TuiMouseEvent, TuiMouseEventType,
    component_key, dispatch_mouse_event, retarget_mouse_event, same_component,
};
use crate::components::scroll_view::ScrollOverscroll;
use crate::layout::{LayoutRect, get_layout_boxes_at, get_scroll_views_at};
use crate::layout_node::get_layout_node;
use crate::tui::TuiInner;

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

static SGR_MOUSE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[<(\d+);(\d+);(\d+)([Mm])$").expect("valid regex"));

/// A decoded wheel event: `direction` is -1 (up) or 1 (down).
pub(crate) struct WheelEvent {
    pub direction: i64,
    pub x: i64,
    pub y: i64,
    pub button: u32,
}

/// A decoded SGR mouse report with zero-based coordinates.
#[derive(Clone, Copy)]
pub(crate) struct SgrMouseEvent {
    pub button: u32,
    pub x: i64,
    pub y: i64,
    pub release: bool,
}

fn legacy_mouse(data: &str) -> Option<(u32, i64, i64)> {
    let rest = data.strip_prefix("\x1b[M")?;
    let chars: Vec<char> = rest.chars().collect();
    if chars.len() != 3 {
        return None;
    }
    Some(((chars[0] as u32).saturating_sub(32), chars[1] as i64 - 33, chars[2] as i64 - 33))
}

pub(crate) fn parse_wheel_event(data: &str) -> Option<WheelEvent> {
    let (button, x, y) = match SGR_MOUSE.captures(data) {
        Some(c) => (c[1].parse::<u32>().ok()?, c[2].parse::<i64>().ok()? - 1, c[3].parse::<i64>().ok()? - 1),
        None => legacy_mouse(data)?,
    };
    if button & 64 == 0 {
        return None;
    }
    let direction = match button & 3 {
        0 => -1,
        1 => 1,
        _ => return None,
    };
    Some(WheelEvent { direction, x, y, button })
}

pub(crate) fn parse_sgr_mouse_event(data: &str) -> Option<SgrMouseEvent> {
    let c = SGR_MOUSE.captures(data)?;
    Some(SgrMouseEvent {
        button: c[1].parse().ok()?,
        x: c[2].parse::<i64>().ok()? - 1,
        y: c[3].parse::<i64>().ok()? - 1,
        release: &c[4] == "m",
    })
}

pub(crate) fn is_mouse_sequence(data: &str) -> bool {
    SGR_MOUSE.is_match(data) || legacy_mouse(data).is_some()
}

fn decode_mouse_button(button: u32) -> TuiMouseButton {
    match button & 3 {
        0 => TuiMouseButton::Left,
        1 => TuiMouseButton::Middle,
        2 => TuiMouseButton::Right,
        _ => TuiMouseButton::None,
    }
}

pub(crate) fn create_mouse_event(
    inner: &TuiInner,
    kind: TuiMouseEventType,
    button: u32,
    x: i64,
    y: i64,
    wheel_delta: Option<i32>,
    click_count: Option<u32>,
) -> TuiMouseEvent {
    TuiMouseEvent {
        kind,
        button: if kind == TuiMouseEventType::Wheel { TuiMouseButton::None } else { decode_mouse_button(button) },
        x: x as i32,
        y: y as i32,
        screen_x: x as i32,
        screen_y: y as i32,
        width: inner.columns().max(1) as i32,
        height: inner.rows().max(1) as i32,
        shift: button & 4 != 0,
        alt: button & 8 != 0,
        ctrl: button & 16 != 0,
        wheel_delta,
        click_count,
    }
}

/// Whether the pointer hit an overlay, and that overlay's dispatch result.
pub(crate) struct OverlayHit {
    pub hit: bool,
    pub result: Option<TuiMouseDispatchResult>,
}

/// Dispatches to the visually topmost overlay under the pointer.
pub(crate) fn dispatch_mouse_to_overlay(inner: &TuiInner, event: &TuiMouseEvent) -> OverlayHit {
    let layouts = inner.state.borrow().rendered_overlay_layouts.clone();
    for layout in layouts.iter().rev() {
        let (col, row) = (layout.col as i32, layout.row as i32);
        if event.screen_x < col
            || event.screen_x >= col + layout.width as i32
            || event.screen_y < row
            || event.screen_y >= row + layout.height as i32
        {
            continue;
        }
        let local = TuiMouseEvent {
            x: event.screen_x - col,
            y: event.screen_y - row,
            width: layout.width as i32,
            height: layout.height as i32,
            ..event.clone()
        };
        let result = dispatch_mouse_event(&layout.entry.component, &local).map(|mut result| {
            if result.result.focus {
                result.focus_target = Some(layout.entry.component.clone());
            }
            result
        });
        return OverlayHit { hit: true, result };
    }
    OverlayHit { hit: false, result: None }
}

pub(crate) fn dispatch_mouse_to_layout(inner: &TuiInner, event: &TuiMouseEvent) -> Option<TuiMouseDispatchResult> {
    let boxes: Vec<(ComponentRef, LayoutRect)> = with_alt(inner, |alt| {
        let layout = alt.current_layout.as_ref()?;
        let boxes = get_layout_boxes_at(layout, event.screen_x as i64, event.screen_y as i64);
        Some(boxes.into_iter().map(|b| (b.component.clone(), b.rect)).collect())
    })
    .flatten()?;
    let mut visited: Vec<usize> = Vec::new();
    for (component, rect) in boxes {
        let key = component_key(&component);
        if visited.contains(&key) {
            continue;
        }
        let passthrough = component.try_borrow().map(|c| !c.intercepts_layout_mouse()).unwrap_or(true);
        if passthrough && get_layout_node(&component).is_some() {
            continue;
        }
        visited.push(key);
        let local = TuiMouseEvent {
            x: event.screen_x - rect.x as i32,
            y: event.screen_y - rect.y as i32,
            width: rect.width as i32,
            height: rect.height as i32,
            ..event.clone()
        };
        if let Some(result) = dispatch_mouse_event(&component, &local) {
            return Some(result);
        }
    }
    None
}

/// Applies focus and capture from a dispatch result. Returns whether to render.
pub(crate) fn apply_mouse_dispatch_result(
    inner: &TuiInner,
    event: &TuiMouseEvent,
    result: &TuiMouseDispatchResult,
) -> bool {
    let target = result.focus_target.clone().unwrap_or_else(|| result.target.component.clone());
    let focus_target = inner.resolve_mouse_focus_target(&target);
    let focused = inner.state.borrow().focused.clone();
    let focus_changed = result.result.focus && !focused.is_some_and(|f| same_component(&f, &focus_target));
    if result.result.focus {
        inner.set_focus(Some(focus_target));
    }
    if result.result.capture {
        with_alt(inner, |alt| alt.mouse_capture = Some(result.target.clone()));
    }
    result.result.render.unwrap_or(
        focus_changed
            || matches!(
                event.kind,
                TuiMouseEventType::Press
                    | TuiMouseEventType::Click
                    | TuiMouseEventType::Drag
                    | TuiMouseEventType::Wheel
            ),
    )
}

fn component_click_count(inner: &TuiInner, target: &TuiMouseDispatchTarget, x: i32, y: i32) -> u32 {
    with_alt(inner, |alt| {
        let now = Instant::now();
        let count = match &alt.last_component_click {
            Some(last)
                if now.duration_since(last.timestamp) <= DOUBLE_CLICK_INTERVAL
                    && same_component(&last.component, &target.component)
                    && last.x == x
                    && last.y == y =>
            {
                last.count % 3 + 1
            }
            _ => 1,
        };
        alt.last_component_click =
            Some(ComponentClick { timestamp: now, count, component: target.component.clone(), x, y });
        count
    })
    .unwrap_or(1)
}

/// Routes a press, release, move, or drag report.
pub(crate) fn handle_mouse_event(inner: &TuiInner, raw: SgrMouseEvent) {
    let motion = raw.button & 32 != 0;
    let kind = if raw.release {
        TuiMouseEventType::Release
    } else if motion {
        if decode_mouse_button(raw.button) == TuiMouseButton::None {
            TuiMouseEventType::Move
        } else {
            TuiMouseEventType::Drag
        }
    } else {
        TuiMouseEventType::Press
    };
    let event = create_mouse_event(inner, kind, raw.button, raw.x, raw.y, None, None);
    let (x, y) = (raw.x as i32, raw.y as i32);

    let gesture = with_alt(inner, |alt| {
        let target = alt.mouse_capture.clone().or_else(|| alt.mouse_press_target.clone())?;
        if alt.mouse_press_point.is_some_and(|point| point != (x, y)) {
            alt.mouse_press_moved = true;
            alt.last_component_click = None;
        }
        Some((target, alt.mouse_press_moved, alt.mouse_press_point))
    })
    .flatten();
    if let Some((target, moved, press_point)) = gesture {
        let mut render = false;
        if let Some(result) = dispatch_mouse_event(&target.component, &retarget_mouse_event(&event, &target)) {
            render = apply_mouse_dispatch_result(inner, &event, &result);
        }
        if raw.release {
            if !moved && press_point == Some((x, y)) {
                let count = component_click_count(inner, &target, x, y);
                let click =
                    create_mouse_event(inner, TuiMouseEventType::Click, raw.button, raw.x, raw.y, None, Some(count));
                if let Some(result) = dispatch_mouse_event(&target.component, &retarget_mouse_event(&click, &target)) {
                    render = apply_mouse_dispatch_result(inner, &click, &result) || render;
                }
            }
            with_alt(inner, |alt| alt.clear_component_mouse_gesture());
        }
        if render {
            inner.request_render(false);
        }
        return;
    }

    if handle_search_mouse_event(inner, raw) {
        return;
    }
    let overlay = dispatch_mouse_to_overlay(inner, &event);
    if !overlay.hit {
        if handle_scroll_to_end_indicator(inner, raw) {
            return;
        }
        let handled = handle_scrollbar_mouse_event(inner, raw);
        if with_alt(inner, |alt| alt.scrollbar_drag.is_none()).unwrap_or(true) {
            update_scrollbar_hover(inner, raw.x, raw.y);
        }
        if handled {
            return;
        }
    } else {
        stop_scrollbar_hover(inner);
    }

    let result = overlay.result.or_else(|| if overlay.hit { None } else { dispatch_mouse_to_layout(inner, &event) });
    if let Some(result) = result {
        let render = apply_mouse_dispatch_result(inner, &event, &result);
        if kind == TuiMouseEventType::Press {
            selection::clear_text_selection(inner);
            with_alt(inner, |alt| {
                alt.mouse_press_target = Some(result.target.clone());
                alt.mouse_press_point = Some((x, y));
                alt.mouse_press_moved = false;
            });
        }
        if render {
            inner.request_render(false);
        }
        return;
    }
    if handle_right_click_paste(inner, raw) {
        return;
    }
    selection::handle_selection_mouse_event(inner, raw);
}

/// Scrolls the views under the pointer, innermost first, passing leftover lines outward.
pub(crate) fn route_wheel(inner: &TuiInner, event: &WheelEvent, delta: i64) {
    let Some((views, primary)) = with_alt(inner, |alt| {
        let views =
            alt.current_layout.as_ref().map(|layout| get_scroll_views_at(layout, event.x, event.y)).unwrap_or_default();
        (views, alt.primary_scroll_view())
    }) else {
        return;
    };
    let mut remaining = delta;
    let mut seen_primary = false;
    for view in views {
        seen_primary |= view.ptr_eq(&primary);
        remaining = view.scroll_by(remaining);
        if remaining == 0 || view.overscroll() == ScrollOverscroll::Contain {
            break;
        }
    }
    if remaining != 0 && !seen_primary {
        primary.scroll_by(remaining);
    }
    update_scrollbar_hover(inner, event.x, event.y);
    inner.request_render(false);
}

fn handle_right_click_paste(inner: &TuiInner, event: SgrMouseEvent) -> bool {
    let Some(paste) = with_alt(inner, |alt| alt.on_right_click_paste.clone()).flatten() else { return false };
    let vscode = inner.env.var("TERM_PROGRAM").is_some_and(|p| p.eq_ignore_ascii_case("vscode"));
    if !cfg!(windows) || vscode || event.release || event.button != 2 {
        return false;
    }
    paste();
    true
}
