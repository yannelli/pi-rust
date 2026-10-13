//! Viewport keys, wheel input, and transcript search.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

use regex::Regex;

use super::{ActiveSearch, SearchSelectionMode, SelectionGranularity, mouse, now_ms, scrollbar, selection, with_alt};
use crate::alt_screen_search::{AltScreenSearchComponent, AltScreenSearchIndex, get_alt_screen_search_match_key};
use crate::component::{ComponentRef, TuiMouseEventType};
use crate::components::scroll_view::ScrollViewScrollToOptions;
use crate::keybindings::get_keybindings;
use crate::keys::is_key_release;
use crate::layout::{LayoutFrame, get_scroll_view_box};
use crate::tui::{OverlayAnchor, OverlayMargin, OverlayOptions, SizeValue, TuiInner};

const FOCUS_IN: &str = "\x1b[I";
const FOCUS_OUT: &str = "\x1b[O";
const PAGE_SCROLL_OVERLAP: i64 = 4;
const ALT_WHEEL_SCROLL_MULTIPLIER: i64 = 5;

static OSC133_PROMPT_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\]133;A(?:\x07|\x1b\\)").expect("valid regex"));

fn primary(inner: &TuiInner) -> Option<crate::components::scroll_view::ScrollViewHandle> {
    with_alt(inner, |alt| alt.primary_scroll_view())
}

pub(crate) fn scroll_by(inner: &TuiInner, lines: i64) {
    if let Some(view) = primary(inner) {
        view.scroll_by(lines);
        inner.request_render(false);
    }
}

pub(crate) fn scroll_to_top(inner: &TuiInner) {
    if let Some(view) = primary(inner) {
        view.scroll_to_start();
        inner.request_render(false);
    }
}

pub(crate) fn scroll_to_bottom(inner: &TuiInner) {
    if let Some(view) = primary(inner) {
        view.scroll_to_end();
        inner.request_render(false);
    }
}

fn scroll_to_prompt(inner: &TuiInner, direction: i64) {
    let found = with_alt(inner, |alt| {
        let layout = alt.current_layout.as_ref()?;
        let view = alt.primary_scroll_view();
        let lines = get_scroll_view_box(layout, &view)?.scroll_content_lines.clone()?;
        let mut row = view.scroll_top() as i64 + direction;
        while row >= 0 && (row as usize) < lines.len() {
            if OSC133_PROMPT_START.is_match(&lines[row as usize]) {
                return Some((view, row));
            }
            row += direction;
        }
        None
    })
    .flatten();
    if let Some((view, row)) = found {
        view.scroll_to(row, ScrollViewScrollToOptions::default());
        inner.request_render(false);
    }
}

fn toggle_search(inner: &TuiInner) {
    if with_alt(inner, |alt| alt.active_search.is_some()).unwrap_or(false) {
        close_search(inner);
        return;
    }
    let weak = inner.weak.clone();
    let style = with_alt(inner, |alt| alt.search_navigation_button_style.clone()).flatten();
    let component = AltScreenSearchComponent::new(
        Box::new(move |query| {
            if let Some(tui) = weak.upgrade() {
                update_search_query(&tui, query);
            }
        }),
        style,
    );
    let box_state = component.state();
    let component: ComponentRef = Rc::new(RefCell::new(component));
    with_alt(inner, |alt| {
        alt.active_search = Some(ActiveSearch {
            box_state,
            index: AltScreenSearchIndex::default(),
            overlay: None,
            query: String::new(),
            matches: Vec::new(),
            selected_index: None,
            selected_key: None,
            anchor_row: alt.primary_scroll_view().scroll_top(),
            selection_mode: SearchSelectionMode::Query,
        });
    });
    let overlay = inner.show_overlay(
        component,
        OverlayOptions {
            anchor: Some(OverlayAnchor::TopRight),
            width: Some(SizeValue::Percent(40.0)),
            min_width: Some(32),
            margin: Some(OverlayMargin::all(1)),
            ..OverlayOptions::default()
        },
    );
    with_alt(inner, |alt| {
        if let Some(search) = alt.active_search.as_mut() {
            search.overlay = Some(overlay);
        }
    });
}

pub(crate) fn close_search(inner: &TuiInner) {
    let Some(search) = with_alt(inner, |alt| alt.active_search.take()).flatten() else { return };
    if let Some(overlay) = search.overlay {
        overlay.hide();
    }
    inner.request_render(false);
}

fn update_search_query(inner: &TuiInner, query: &str) {
    let changed = with_alt(inner, |alt| {
        let fallback_row = alt.primary_scroll_view().scroll_top();
        let Some(search) = alt.active_search.as_mut() else { return false };
        if query == search.query {
            return false;
        }
        let selected_row = search.selected_index.and_then(|i| search.matches.get(i)).and_then(|m| m.segments.first());
        search.anchor_row = selected_row.map_or(fallback_row, |segment| segment.row);
        search.query = query.to_string();
        search.selection_mode = SearchSelectionMode::Query;
        search.box_state.borrow_mut().set_result(None, 0);
        true
    })
    .unwrap_or(false);
    if changed {
        inner.request_render(false);
    }
}

pub(super) fn navigate_search(inner: &TuiInner, direction: i32) {
    let changed = with_alt(inner, |alt| {
        let Some(search) = alt.active_search.as_mut().filter(|s| !s.query.is_empty()) else { return false };
        search.selection_mode = if direction < 0 { SearchSelectionMode::Previous } else { SearchSelectionMode::Next };
        true
    })
    .unwrap_or(false);
    if changed {
        inner.request_render(false);
    }
}

fn search_overlay_focused(inner: &TuiInner) -> bool {
    let overlay = with_alt(inner, |alt| alt.active_search.as_ref().and_then(|s| s.overlay.clone())).flatten();
    overlay.is_some_and(|overlay| overlay.is_focused())
}

fn should_defer_to_overlay(inner: &TuiInner) -> bool {
    inner.is_overlay_focused() && !search_overlay_focused(inner)
}

/// Refreshes search matches for a new frame. Returns true when the view scrolled to reveal the
/// selected match, so the frame must be laid out again.
pub(crate) fn refresh_search(inner: &TuiInner, layout: &LayoutFrame) -> bool {
    with_alt(inner, |alt| {
        let implicit = alt.implicit_scroll.clone();
        let Some(search) = alt.active_search.as_mut() else { return false };
        let view = layout.primary_scroll_view.clone().unwrap_or(implicit);
        let layout_box = get_scroll_view_box(layout, &view);
        let lines = layout_box.and_then(|b| b.scroll_content_lines.clone());
        let Some(lines) = lines.filter(|_| !search.query.trim().is_empty()) else {
            search.matches.clear();
            search.selected_index = None;
            search.selected_key = None;
            search.selection_mode = SearchSelectionMode::Retain;
            search.box_state.borrow_mut().set_result(None, 0);
            return false;
        };
        let reveal = search.selection_mode != SearchSelectionMode::Retain;
        let result = search.index.search(&lines, &search.query);
        let changed = result.changed;
        search.matches = result.matches.to_vec();
        if !changed && search.selection_mode == SearchSelectionMode::Retain {
            return false;
        }
        let matches = &search.matches;
        let exact = if changed {
            search
                .selected_key
                .as_ref()
                .and_then(|key| matches.iter().position(|m| get_alt_screen_search_match_key(m) == *key))
        } else {
            search.selected_index
        };
        let len = matches.len();
        let clamped_previous = search.selected_index.map(|i| i.min(len.saturating_sub(1)));
        let selected = if len == 0 {
            None
        } else {
            match search.selection_mode {
                SearchSelectionMode::Query => {
                    let first_at_or_after =
                        matches.partition_point(|m| m.segments.first().map_or(0, |s| s.row) < search.anchor_row);
                    Some(if first_at_or_after < len { first_at_or_after } else { 0 })
                }
                SearchSelectionMode::Next => Some(exact.or(clamped_previous).map_or(0, |base| (base + 1) % len)),
                SearchSelectionMode::Previous => {
                    Some(exact.or(clamped_previous).map_or(len - 1, |base| (base + len - 1) % len))
                }
                SearchSelectionMode::Retain => Some(exact.unwrap_or_else(|| clamped_previous.unwrap_or(0))),
            }
        };
        search.selected_index = selected;
        search.selected_key = selected.map(|i| get_alt_screen_search_match_key(&matches[i]));
        search.selection_mode = SearchSelectionMode::Retain;
        search.box_state.borrow_mut().set_result(selected, len);
        if !reveal {
            return false;
        }
        let Some(found) = selected.map(|i| &search.matches[i]) else { return false };
        let (Some(first), Some(last)) = (found.segments.first(), found.segments.last()) else { return false };
        let viewport_height = view.viewport_height();
        if layout_box.is_none() || viewport_height == 0 {
            return false;
        }
        let before = view.scroll_top();
        let visible_bottom = before + viewport_height - 1;
        let mut target = before as i64;
        if first.row < before || last.row > visible_bottom {
            target = first.row as i64 - (viewport_height / 3) as i64;
        }
        view.scroll_to(target, ScrollViewScrollToOptions { disable_follow: true });
        view.scroll_top() != before
    })
    .unwrap_or(false)
}

fn reset_on_focus_out(inner: &TuiInner) {
    let had_selection = with_alt(inner, |alt| alt.selection_press_active).unwrap_or(false);
    let had_non_empty = had_selection && selection::selection_bounds(inner).is_some();
    selection::stop_selection_auto_scroll(inner);
    scrollbar::stop_scrollbar_hover(inner);
    let hover_changed = with_alt(inner, |alt| {
        alt.selection_press_active = false;
        let changed =
            alt.active_search.as_ref().is_some_and(|s| s.box_state.borrow_mut().set_hovered_navigation_direction(None));
        alt.scrollbar_drag = None;
        alt.pressed_url = None;
        alt.selection_dragged = false;
        alt.clear_component_mouse_gesture();
        alt.last_component_click = None;
        if had_selection {
            alt.selection_anchor = None;
            alt.selection_focus = None;
            alt.selection_granularity = SelectionGranularity::Character;
            alt.selection_initial_range = None;
        }
        alt.last_click = None;
        changed
    })
    .unwrap_or(false);
    if hover_changed || (had_selection && had_non_empty) {
        inner.request_render(false);
    }
}

/// The fullscreen input listener. Returns true when the input was consumed.
pub(crate) fn handle_viewport_input(inner: &TuiInner, data: &str) -> bool {
    if data == FOCUS_OUT {
        reset_on_focus_out(inner);
        return true;
    }
    if data == FOCUS_IN {
        return true;
    }
    if let Some(wheel) = mouse::parse_wheel_event(data) {
        let lines = with_alt(inner, |alt| alt.wheel_scroll.next(wheel.direction as i32, now_ms())).unwrap_or(1) as i64;
        let multiplier = if wheel.button & 8 != 0 { ALT_WHEEL_SCROLL_MULTIPLIER } else { 1 };
        let delta = wheel.direction * lines * multiplier;
        let event = mouse::create_mouse_event(
            inner,
            TuiMouseEventType::Wheel,
            wheel.button,
            wheel.x,
            wheel.y,
            Some(delta as i32),
            None,
        );
        let overlay = mouse::dispatch_mouse_to_overlay(inner, &event);
        let result =
            overlay.result.or_else(|| if overlay.hit { None } else { mouse::dispatch_mouse_to_layout(inner, &event) });
        if let Some(result) = result {
            if mouse::apply_mouse_dispatch_result(inner, &event, &result) {
                inner.request_render(false);
            }
            return true;
        }
        if should_defer_to_overlay(inner) {
            return false;
        }
        mouse::route_wheel(inner, &wheel, delta);
        return true;
    }
    if let Some(event) = mouse::parse_sgr_mouse_event(data) {
        mouse::handle_mouse_event(inner, event);
        return true;
    }
    if mouse::is_mouse_sequence(data) {
        return true;
    }

    let keybindings = get_keybindings();
    let release = is_key_release(data);
    if keybindings.matches(data, "tui.altScreen.search") {
        if !release {
            toggle_search(inner);
        }
        return true;
    }
    if search_overlay_focused(inner) {
        for (id, direction) in [("tui.altScreen.searchNext", 1), ("tui.altScreen.searchPrevious", -1)] {
            if keybindings.matches(data, id) {
                if !release {
                    navigate_search(inner, direction);
                }
                return true;
            }
        }
        if keybindings.matches(data, "tui.altScreen.searchClose") {
            if !release {
                close_search(inner);
            }
            return true;
        }
    }
    if should_defer_to_overlay(inner) {
        return false;
    }
    let viewport_height = primary(inner).map_or(0, |v| v.viewport_height()) as i64;
    let page = (viewport_height - PAGE_SCROLL_OVERLAP).max(1);
    let half = (viewport_height / 2).max(1);
    let actions: [(&str, &dyn Fn()); 10] = [
        ("tui.altScreen.pageUp", &|| scroll_by(inner, -page)),
        ("tui.altScreen.pageDown", &|| scroll_by(inner, page)),
        ("tui.altScreen.halfPageUp", &|| scroll_by(inner, -half)),
        ("tui.altScreen.halfPageDown", &|| scroll_by(inner, half)),
        ("tui.altScreen.lineUp", &|| scroll_by(inner, -1)),
        ("tui.altScreen.lineDown", &|| scroll_by(inner, 1)),
        ("tui.altScreen.previousPrompt", &|| scroll_to_prompt(inner, -1)),
        ("tui.altScreen.nextPrompt", &|| scroll_to_prompt(inner, 1)),
        ("tui.altScreen.top", &|| scroll_to_top(inner)),
        ("tui.altScreen.bottom", &|| scroll_to_bottom(inner)),
    ];
    for (id, action) in actions {
        if keybindings.matches(data, id) {
            if !release {
                action();
            }
            return true;
        }
    }
    false
}
