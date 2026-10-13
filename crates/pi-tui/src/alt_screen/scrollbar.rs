//! Scrollbar hover and dragging, the jump-to-end indicator, and search box buttons.

use super::mouse::SgrMouseEvent;
use super::{SelectionGranularity, selection, viewport, with_alt};
use crate::components::scroll_view::{ScrollViewHandle, ScrollViewScrollToOptions};
use crate::layout::{ScrollbarGeometry, get_scroll_view_box, get_scroll_views_at, get_scrollbar_geometry};
use crate::tui::TuiInner;

pub(super) fn handle_scroll_to_end_indicator(inner: &TuiInner, event: SgrMouseEvent) -> bool {
    let Some((row, column, width)) = with_alt(inner, |alt| alt.scroll_to_end_indicator_rect).flatten() else {
        return false;
    };
    if event.release || event.button & 32 != 0 || event.button & 3 != 0 {
        return false;
    }
    if event.y != row || event.x < column || event.x >= column + width {
        return false;
    }
    viewport::scroll_to_bottom(inner);
    true
}

fn scrollbar_target_at(
    inner: &TuiInner,
    x: i64,
    y: i64,
    include_hidden_auto: bool,
) -> Option<(ScrollViewHandle, ScrollbarGeometry)> {
    if inner.has_visible_overlay() {
        return None;
    }
    with_alt(inner, |alt| {
        let layout = alt.current_layout.as_ref()?;
        get_scroll_views_at(layout, x, y).into_iter().find_map(|view| {
            let layout_box = get_scroll_view_box(layout, &view)?;
            let geometry = get_scrollbar_geometry(layout_box, include_hidden_auto)?;
            let on_track =
                x == geometry.column && y >= geometry.track_top && y < geometry.track_top + geometry.track_height;
            on_track.then_some((view, geometry))
        })
    })
    .flatten()
}

fn set_scrollbar_hover(inner: &TuiInner, view: Option<ScrollViewHandle>) {
    let previous = with_alt(inner, |alt| {
        let same = match (&alt.scrollbar_hover, &view) {
            (Some(a), Some(b)) => a.ptr_eq(b),
            (None, None) => true,
            _ => false,
        };
        if same { None } else { Some(std::mem::replace(&mut alt.scrollbar_hover, view.clone())) }
    })
    .flatten();
    let Some(previous) = previous else { return };
    if let Some(previous) = previous {
        previous.set_scrollbar_active(false);
    }
    if let Some(view) = view {
        view.set_scrollbar_active(true);
    }
}

pub(crate) fn update_scrollbar_hover(inner: &TuiInner, x: i64, y: i64) {
    set_scrollbar_hover(inner, scrollbar_target_at(inner, x, y, true).map(|(view, _)| view));
}

pub(crate) fn stop_scrollbar_hover(inner: &TuiInner) {
    set_scrollbar_hover(inner, None);
}

fn scroll_to_pointer(view: &ScrollViewHandle, geometry: &ScrollbarGeometry, pointer_y: i64, grab_offset: i64) {
    let max_thumb_offset = geometry.track_height - geometry.thumb_height;
    let thumb_offset = (pointer_y - geometry.track_top - grab_offset).clamp(0, max_thumb_offset.max(0));
    let scroll_top = if max_thumb_offset == 0 {
        0
    } else {
        ((thumb_offset as f64 / max_thumb_offset as f64) * geometry.max_scroll_top as f64).round() as i64
    };
    view.scroll_to(scroll_top, ScrollViewScrollToOptions::default());
}

pub(super) fn handle_scrollbar_mouse_event(inner: &TuiInner, event: SgrMouseEvent) -> bool {
    if let Some((view, grab_offset)) = with_alt(inner, |alt| alt.scrollbar_drag.clone()).flatten() {
        if event.release {
            with_alt(inner, |alt| alt.scrollbar_drag = None);
            return true;
        }
        let geometry = with_alt(inner, |alt| {
            let layout = alt.current_layout.as_ref()?;
            get_scroll_view_box(layout, &view).and_then(|b| get_scrollbar_geometry(b, false))
        })
        .flatten();
        if let Some(geometry) = geometry {
            scroll_to_pointer(&view, &geometry, event.y, grab_offset);
        }
        return true;
    }
    if event.release || event.button & 32 != 0 || event.button & 3 != 0 {
        return false;
    }
    let Some((view, geometry)) = scrollbar_target_at(inner, event.x, event.y, false) else { return false };
    selection::stop_selection_auto_scroll(inner);
    with_alt(inner, |alt| {
        alt.selection_press_active = false;
        alt.selection_anchor = None;
        alt.selection_focus = None;
        alt.selection_granularity = SelectionGranularity::Character;
        alt.selection_initial_range = None;
        alt.last_click = None;
        alt.pressed_url = None;
        alt.selection_dragged = false;
    });
    set_scrollbar_hover(inner, Some(view.clone()));
    let on_thumb = event.y >= geometry.thumb_top && event.y < geometry.thumb_top + geometry.thumb_height;
    let grab_offset = if on_thumb { event.y - geometry.thumb_top } else { geometry.thumb_height / 2 };
    if !on_thumb {
        scroll_to_pointer(&view, &geometry, event.y, grab_offset);
    }
    with_alt(inner, |alt| alt.scrollbar_drag = Some((view, grab_offset)));
    true
}

/// Hover and clicks on the search box navigation buttons. Returns true when a click navigated.
pub(super) fn handle_search_mouse_event(inner: &TuiInner, event: SgrMouseEvent) -> bool {
    let Some((box_state, overlay)) =
        with_alt(inner, |alt| alt.active_search.as_ref().map(|s| (s.box_state.clone(), s.overlay.clone()))).flatten()
    else {
        return false;
    };
    let direction = overlay.and_then(|o| o.get_bounds()).and_then(|bounds| {
        let (col, row) = (bounds.col as i64, bounds.row as i64);
        let inside = event.x >= col
            && event.x < col + bounds.width as i64
            && event.y >= row
            && event.y < row + bounds.height as i64;
        if !inside {
            return None;
        }
        box_state.borrow().get_navigation_direction_at((event.y - row) as usize, (event.x - col) as usize)
    });
    if box_state.borrow_mut().set_hovered_navigation_direction(direction) {
        inner.request_render(false);
    }
    let Some(direction) = direction else { return false };
    if event.release || event.button & 32 != 0 || event.button & 3 != 0 {
        return false;
    }
    viewport::navigate_search(inner, direction);
    true
}
