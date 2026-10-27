//! Application-owned text selection: character, word, and line granularity, auto-scroll while
//! dragging, OSC 8 link clicks, and copy-on-select.

use std::time::{Duration, Instant};

use super::mouse::{
    SgrMouseEvent, apply_mouse_dispatch_result, create_mouse_event, dispatch_mouse_to_layout, dispatch_mouse_to_overlay,
};
use super::{
    AltScreenState, ClickTarget, SelectionGranularity, SelectionPoint, SelectionRange, public, same_view, with_alt,
};
use crate::component::TuiMouseEventType;
use crate::components::scroll_view::ScrollViewHandle;
use crate::layout::{LayoutBox, get_scroll_view_box, get_scroll_views_at};
use crate::tui::TuiInner;
use crate::ui_loop;
use crate::utils::{
    get_grapheme_cell_range, get_osc8_link_at_column, slice_by_column, strip_terminal_sequences, visible_width,
    word_segments,
};

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
/// Fullscreen owns selection, so it uses iTerm2's default "characters considered part of a word" set:
/// https://github.com/gnachman/iTerm2/blob/7f4ff739721f95d0b93003d8701c4523a7edf261/sources/Settings/iTermPreferences.m#L743
const WORD_SELECTION_JOINERS: [&str; 7] = ["/", "-", "+", "\\", "~", "_", "."];

impl AltScreenState {
    fn visible_rows(&self, view: &ScrollViewHandle, rows: usize) -> Option<(i64, i64, &LayoutBox)> {
        let layout = self.current_layout.as_ref()?;
        let layout_box = get_scroll_view_box(layout, view)?;
        if layout_box.rect.height <= 0 || layout_box.clip.height <= 0 {
            return None;
        }
        let top = 0.max(layout_box.rect.y).max(layout_box.clip.y);
        let bottom = (rows as i64 - 1)
            .min(layout_box.rect.y + layout_box.rect.height - 1)
            .min(layout_box.clip.y + layout_box.clip.height - 1);
        Some((top, bottom, layout_box))
    }

    fn scroll_selection_point(&self, view: &ScrollViewHandle, x: i64, y: i64, rows: usize) -> Option<SelectionPoint> {
        let (top, bottom, layout_box) = self.visible_rows(view, rows)?;
        if bottom < top {
            return None;
        }
        let pointer_row = y.clamp(top, bottom);
        let max_content_row = (layout_box.scroll_content_lines.as_ref().map_or(1, |l| l.len()) as i64 - 1).max(0);
        Some(SelectionPoint {
            row: (view.scroll_top() as i64 + pointer_row - layout_box.rect.y).clamp(0, max_content_row),
            col: (x - layout_box.rect.x).clamp(0, (layout_box.rect.width - 1).max(0)),
            scroll_view: Some(view.clone()),
            boundary: false,
        })
    }

    fn selection_point(&self, x: i64, y: i64, view: Option<&ScrollViewHandle>, size: (usize, usize)) -> SelectionPoint {
        if let Some(point) = view.and_then(|view| self.scroll_selection_point(view, x, y, size.1)) {
            return point;
        }
        SelectionPoint {
            row: y.clamp(0, (size.1 as i64 - 1).max(0)),
            col: x.clamp(0, (size.0 as i64 - 1).max(0)),
            scroll_view: None,
            boundary: false,
        }
    }

    fn source_line(&self, point: &SelectionPoint) -> String {
        if let (Some(view), Some(layout)) = (&point.scroll_view, &self.current_layout)
            && let Some(lines) = get_scroll_view_box(layout, view).and_then(|b| b.scroll_content_lines.clone())
        {
            return lines.get(point.row as usize).cloned().unwrap_or_default();
        }
        self.previous_screen.get(point.row.max(0) as usize).cloned().unwrap_or_default()
    }

    fn word_selection(&self, point: &SelectionPoint) -> Option<SelectionRange> {
        let line = strip_terminal_sequences(&self.source_line(point));
        let mut segments: Vec<(i64, i64, bool, bool)> = Vec::new();
        let mut start = 0i64;
        for segment in word_segments(&line) {
            let end = start + visible_width(segment.segment) as i64;
            let joiner = WORD_SELECTION_JOINERS.contains(&segment.segment);
            segments.push((start, end, segment.is_word_like || joiner, joiner));
            start = end;
        }
        let clicked = segments.iter().position(|(s, e, _, _)| point.col >= *s && point.col < *e)?;
        let can_join = |l: &(i64, i64, bool, bool), r: &(i64, i64, bool, bool)| l.2 && r.2 && (l.3 || r.3);
        let mut first = clicked;
        while first > 0 && can_join(&segments[first - 1], &segments[first]) {
            first -= 1;
        }
        let mut last = clicked;
        while last + 1 < segments.len() && can_join(&segments[last], &segments[last + 1]) {
            last += 1;
        }
        Some(SelectionRange {
            start: SelectionPoint { col: segments[first].0, ..point.clone() },
            end: SelectionPoint { col: segments[last].1, boundary: true, ..point.clone() },
        })
    }

    fn line_selection(&self, point: &SelectionPoint) -> SelectionRange {
        let width = visible_width(&self.source_line(point)) as i64;
        SelectionRange {
            start: SelectionPoint { col: 0, ..point.clone() },
            end: SelectionPoint { col: width, boundary: true, ..point.clone() },
        }
    }

    fn update_selection_focus(&mut self, point: SelectionPoint) {
        let initial = self.selection_initial_range.clone();
        let Some(initial) = initial.filter(|_| self.selection_granularity != SelectionGranularity::Character) else {
            self.selection_focus = Some(point);
            return;
        };
        let range = match self.selection_granularity {
            SelectionGranularity::Word => self.word_selection(&point),
            _ => Some(self.line_selection(&point)),
        };
        let Some(range) = range else { return };
        let before = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if before {
            self.selection_anchor = Some(initial.end);
            self.selection_focus = Some(range.start);
        } else {
            self.selection_anchor = Some(initial.start);
            self.selection_focus = Some(range.end);
        }
    }

    fn click_count(&mut self, point: &SelectionPoint, word: Option<&SelectionRange>) -> u32 {
        let now = Instant::now();
        let count = match (word, &self.last_click) {
            (Some(word), Some(last))
                if now.duration_since(last.timestamp) <= DOUBLE_CLICK_INTERVAL
                    && last.row == point.row
                    && same_view(&last.scroll_view, &point.scroll_view)
                    && last.word_start == word.start.col
                    && last.word_end == word.end.col =>
            {
                last.count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = word.map(|word| ClickTarget {
            timestamp: now,
            count,
            row: point.row,
            scroll_view: point.scroll_view.clone(),
            word_start: word.start.col,
            word_end: word.end.col,
        });
        count
    }

    pub(super) fn bounds(&self) -> Option<SelectionRange> {
        let (anchor, focus) = (self.selection_anchor.as_ref()?, self.selection_focus.as_ref()?);
        if !same_view(&anchor.scroll_view, &focus.scroll_view) || (anchor.row == focus.row && anchor.col == focus.col) {
            return None;
        }
        let anchor_first = anchor.row < focus.row || (anchor.row == focus.row && anchor.col < focus.col);
        let (start, end) = if anchor_first { (anchor, focus) } else { (focus, anchor) };
        Some(SelectionRange { start: start.clone(), end: end.clone() })
    }

    fn clear_selection_state(&mut self) {
        self.selection_press_active = false;
        self.selection_anchor = None;
        self.selection_focus = None;
        self.selection_granularity = SelectionGranularity::Character;
        self.selection_initial_range = None;
        self.pressed_url = None;
        self.selection_dragged = false;
    }
}

pub(crate) fn selection_bounds(inner: &TuiInner) -> Option<SelectionRange> {
    with_alt(inner, |alt| alt.bounds()).flatten()
}

/// Selected cell range of one row: the start column through the end column (exclusive).
pub(super) fn selection_columns(
    line: &str,
    row: i64,
    selection: &SelectionRange,
    min: i64,
    max: Option<i64>,
) -> (i64, i64) {
    let line_width = visible_width(line) as i64;
    let max = max.unwrap_or(line_width);
    let mut start = min.max(0);
    let mut end = line_width.min(max);
    if row == selection.start.row {
        let col = selection.start.col;
        start = get_grapheme_cell_range(line, col.max(0) as usize).map_or(col.min(line_width), |r| r.start as i64);
    }
    if row == selection.end.row {
        let col = selection.end.col;
        end = if selection.end.boundary {
            col.min(line_width)
        } else {
            get_grapheme_cell_range(line, col.max(0) as usize).map_or((col + 1).min(line_width), |r| r.end as i64)
        };
    }
    (start.max(min), end.min(max))
}

/// Text of the active selection, or `None` when nothing is selected.
pub(crate) fn active_selection_text(inner: &TuiInner) -> Option<String> {
    with_alt(inner, |alt| {
        let selection = alt.bounds()?;
        let lines: Vec<String> = match &selection.start.scroll_view {
            Some(view) => {
                let layout = alt.current_layout.as_ref()?;
                get_scroll_view_box(layout, view)?.scroll_content_lines.as_ref()?.to_vec()
            }
            None => alt.previous_screen.clone(),
        };
        let mut out = Vec::new();
        for row in selection.start.row..=selection.end.row {
            let line = lines.get(row.max(0) as usize).map(String::as_str).unwrap_or("");
            let (start, end) = selection_columns(line, row, &selection, 0, None);
            let sliced = slice_by_column(line, start.max(0) as usize, (end - start).max(0) as usize, true);
            out.push(strip_terminal_sequences(&sliced).trim_end().to_string());
        }
        let text = out.join("\n");
        (!text.is_empty()).then_some(text)
    })
    .flatten()
}

pub(crate) fn clear_text_selection(inner: &TuiInner) {
    stop_selection_auto_scroll(inner);
    with_alt(inner, AltScreenState::clear_selection_state);
}

pub(crate) fn stop_selection_auto_scroll(inner: &TuiInner) {
    with_alt(inner, |alt| {
        if let Some(timer) = alt.selection_auto_scroll_timer.take() {
            ui_loop::clear_timer(timer);
        }
        alt.selection_auto_scroll_direction = 0;
        alt.selection_drag_pointer = None;
    });
}

fn update_selection_auto_scroll(inner: &TuiInner, event: SgrMouseEvent) {
    let rows = inner.rows();
    let decision = with_alt(inner, |alt| {
        let view = alt.selection_anchor.as_ref().and_then(|a| a.scroll_view.clone())?;
        let (top, bottom, _) = alt.visible_rows(&view, rows)?;
        alt.selection_drag_pointer = Some((event.x, event.y));
        alt.selection_auto_scroll_direction = if event.y <= top { -1 } else { i64::from(event.y >= bottom) };
        Some((alt.selection_auto_scroll_direction != 0, alt.selection_auto_scroll_timer.is_some()))
    })
    .flatten();
    match decision {
        Some((true, false)) => {
            let weak = inner.weak.clone();
            let timer = ui_loop::set_interval(AUTO_SCROLL_INTERVAL, move || {
                if let Some(tui) = weak.upgrade() {
                    auto_scroll_selection(&tui);
                }
            });
            with_alt(inner, |alt| alt.selection_auto_scroll_timer = Some(timer));
        }
        Some((true, true)) => {}
        _ => stop_selection_auto_scroll(inner),
    }
}

fn auto_scroll_selection(inner: &TuiInner) {
    let rows = inner.rows();
    let state = with_alt(inner, |alt| {
        let view = alt.selection_anchor.as_ref().and_then(|a| a.scroll_view.clone())?;
        let pointer = alt.selection_drag_pointer?;
        let direction = alt.selection_auto_scroll_direction;
        (direction != 0).then_some((view, pointer, direction))
    })
    .flatten();
    let Some((view, (x, y), direction)) = state else {
        stop_selection_auto_scroll(inner);
        return;
    };
    if view.scroll_by(direction) == direction {
        stop_selection_auto_scroll(inner);
        return;
    }
    with_alt(inner, |alt| {
        if let Some(point) = alt.scroll_selection_point(&view, x, y, rows) {
            alt.update_selection_focus(point);
        }
    });
    inner.request_render(false);
}

/// Selection handling for mouse reports that no component took.
pub(crate) fn handle_selection_mouse_event(inner: &TuiInner, event: SgrMouseEvent) {
    let button = event.button & 3;
    if button != 0 && !(event.release && button == 3) {
        return;
    }
    let size = (inner.columns(), inner.rows());
    if event.release {
        release_selection(inner, event, size);
        return;
    }
    if event.button & 32 != 0 {
        let dragging = with_alt(inner, |alt| {
            if !alt.selection_press_active || alt.selection_anchor.is_none() {
                return false;
            }
            let view = alt.selection_anchor.as_ref().and_then(|a| a.scroll_view.clone());
            let point = alt.selection_point(event.x, event.y, view.as_ref(), size);
            alt.selection_dragged = true;
            alt.last_click = None;
            alt.pressed_url = None;
            alt.update_selection_focus(point);
            true
        })
        .unwrap_or(false);
        if dragging {
            update_selection_auto_scroll(inner, event);
            inner.request_render(false);
        }
        return;
    }
    stop_selection_auto_scroll(inner);
    let has_overlay = inner.has_visible_overlay();
    with_alt(inner, |alt| {
        alt.selection_press_active = true;
        let view = if has_overlay {
            None
        } else {
            alt.current_layout
                .as_ref()
                .and_then(|layout| get_scroll_views_at(layout, event.x, event.y).into_iter().next())
        };
        let anchor = alt.selection_point(event.x, event.y, view.as_ref(), size);
        let word = alt.word_selection(&anchor);
        let clicks = alt.click_count(&anchor, word.as_ref());
        let range = match clicks {
            2 => word,
            3 => Some(alt.line_selection(&anchor)),
            _ => None,
        };
        alt.selection_granularity = match (&range, clicks) {
            (Some(_), 2) => SelectionGranularity::Word,
            (Some(_), _) => SelectionGranularity::Line,
            (None, _) => SelectionGranularity::Character,
        };
        alt.selection_initial_range = range.clone();
        alt.selection_anchor = Some(range.as_ref().map_or_else(|| anchor.clone(), |r| r.start.clone()));
        alt.selection_focus = Some(range.as_ref().map_or_else(|| anchor.clone(), |r| r.end.clone()));
        alt.selection_dragged = false;
        alt.pressed_url = if range.is_some() {
            None
        } else {
            let row = event.y.clamp(0, (size.1 as i64 - 1).max(0)) as usize;
            let col = event.x.clamp(0, (size.0 as i64 - 1).max(0)) as usize;
            get_osc8_link_at_column(alt.previous_screen.get(row).map(String::as_str).unwrap_or(""), col)
        };
    });
    inner.request_render(false);
}

fn release_selection(inner: &TuiInner, event: SgrMouseEvent, size: (usize, usize)) {
    let pressed = with_alt(inner, |alt| std::mem::replace(&mut alt.selection_press_active, false)).unwrap_or(false);
    if !pressed {
        return;
    }
    stop_selection_auto_scroll(inner);
    let release = with_alt(inner, |alt| {
        let anchor = alt.selection_anchor.clone()?;
        let point = alt.selection_point(event.x, event.y, anchor.scroll_view.as_ref(), size);
        alt.update_selection_focus(point.clone());
        let is_click = !alt.selection_dragged
            && same_view(&anchor.scroll_view, &point.scroll_view)
            && anchor.row == point.row
            && anchor.col == point.col;
        let url = alt.pressed_url.take().filter(|_| is_click);
        let open = url.and_then(|url| alt.open_url.clone().map(|open| (open, url)));
        if open.is_some() {
            alt.selection_anchor = None;
            alt.selection_focus = None;
        }
        Some((is_click, open, alt.last_click.as_ref().map_or(1, |c| c.count), alt.copy_on_select))
    })
    .flatten();
    let Some((is_click, open, click_count, copy_on_select)) = release else { return };
    if let Some((open, url)) = open {
        open(&url);
        inner.request_render(false);
        return;
    }
    if is_click {
        let click = create_mouse_event(
            inner,
            TuiMouseEventType::Click,
            event.button,
            event.x,
            event.y,
            None,
            Some(click_count),
        );
        let overlay = dispatch_mouse_to_overlay(inner, &click);
        let result =
            overlay.result.or_else(|| if overlay.hit { None } else { dispatch_mouse_to_layout(inner, &click) });
        if let Some(result) = result {
            let render = apply_mouse_dispatch_result(inner, &click, &result);
            clear_text_selection(inner);
            if render {
                inner.request_render(false);
            }
            return;
        }
    }
    if copy_on_select && let Some(text) = active_selection_text(inner) {
        public::copy_text_to_clipboard(inner, text);
    }
    inner.request_render(false);
}
