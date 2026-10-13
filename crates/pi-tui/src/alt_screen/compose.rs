//! Frame decorations: search highlights, selection highlight, jump-to-end indicator, flashes.

use std::collections::BTreeMap;

use super::selection::{selection_bounds, selection_columns};
use super::with_alt;
use crate::components::scroll_view::StyleFn;
use crate::layout::{LayoutFrame, get_scroll_view_box, get_scrollbar_geometry};
use crate::terminal_image::is_image_line;
use crate::tui::{TuiInner, composite_tui_line};
use crate::utils::{extract_ansi_code, slice_by_column, truncate_to_width_with, visible_width};

/// Styles the plain text runs of `text`, keeping its escape codes in place.
fn highlight_plain_runs(text: &str, style: &StyleFn) -> String {
    let mut result = String::new();
    let mut plain_start = 0;
    let mut index = 0;
    while index < text.len() {
        match extract_ansi_code(text, index) {
            Some(code) => {
                if index > plain_start {
                    result.push_str(&style(&text[plain_start..index]));
                }
                result.push_str(code);
                index += code.len();
                plain_start = index;
            }
            None => index += text[index..].chars().next().map_or(1, char::len_utf8),
        }
    }
    if plain_start < text.len() {
        result.push_str(&style(&text[plain_start..]));
    }
    result
}

pub(super) fn apply_search_highlights(inner: &TuiInner, screen: Vec<String>, layout: &LayoutFrame) -> Vec<String> {
    let columns = inner.columns() as i64;
    let ranges = with_alt(inner, |alt| {
        let search = alt.active_search.as_ref()?;
        let selected = search.selected_index?;
        if search.matches.is_empty() {
            return None;
        }
        let view = layout.primary_scroll_view.clone().unwrap_or_else(|| alt.implicit_scroll.clone());
        let layout_box = get_scroll_view_box(layout, &view)?;
        let (rect, clip) = (layout_box.rect, layout_box.clip);
        let scrollbar_column = get_scrollbar_geometry(layout_box, false).map(|g| g.column);
        let min_row = 0.max(rect.y).max(clip.y);
        let max_row = (screen.len() as i64).min(rect.y + rect.height).min(clip.y + clip.height);
        let min_col = 0.max(rect.x).max(clip.x);
        let max_col =
            columns.min(rect.x + rect.width).min(clip.x + clip.width).min(scrollbar_column.unwrap_or(i64::MAX));
        let top = view.scroll_top() as i64;
        let min_content_row = top + min_row - rect.y;
        let max_content_row = top + max_row - rect.y - 1;
        let first =
            search.matches.partition_point(|m| m.segments.last().map_or(-1, |s| s.row as i64) < min_content_row);
        let mut by_row: BTreeMap<i64, Vec<(i64, i64, bool)>> = BTreeMap::new();
        for (index, found) in search.matches.iter().enumerate().skip(first) {
            if found.segments.first().map_or(0, |s| s.row as i64) > max_content_row {
                break;
            }
            for segment in &found.segments {
                let row = rect.y + segment.row as i64 - top;
                if row < min_row || row >= max_row {
                    continue;
                }
                let start = min_col.max(rect.x + segment.start_col as i64);
                let end = max_col.min(rect.x + segment.end_col as i64);
                if end > start {
                    by_row.entry(row).or_default().push((start, end, index == selected));
                }
            }
        }
        Some((by_row, alt.search_match_style.clone(), alt.search_current_match_style.clone()))
    })
    .flatten();
    let Some((by_row, match_style, current_style)) = ranges else { return screen };
    let mut result = screen;
    for (row, mut ranges) in by_row {
        let Some(line) = result.get(row as usize).cloned() else { continue };
        if is_image_line(&line) {
            continue;
        }
        let mut line = line;
        let line_width = visible_width(&line) as i64;
        ranges.sort_by_key(|range| std::cmp::Reverse(range.0));
        for (start, end, current) in ranges {
            let (start, end) = (start.min(line_width) as usize, end.min(line_width) as usize);
            if end <= start {
                continue;
            }
            let before = slice_by_column(&line, 0, start, true);
            let highlighted = slice_by_column(&line, start, end - start, true);
            let after = slice_by_column(&line, end, (line_width as usize).saturating_sub(end), true);
            let style = if current { &current_style } else { &match_style };
            line = format!("{before}{}{after}", highlight_plain_runs(&highlighted, style));
        }
        result[row as usize] = line;
    }
    result
}

pub(super) fn composite_scroll_to_end_indicator(
    inner: &TuiInner,
    screen: Vec<String>,
    layout: &LayoutFrame,
    width: usize,
) -> Vec<String> {
    let setup = with_alt(inner, |alt| {
        alt.scroll_to_end_indicator_rect = None;
        let view = layout.primary_scroll_view.clone().unwrap_or_else(|| alt.implicit_scroll.clone());
        let indicator = alt.scroll_to_end_indicator.clone()?;
        if !view.follow_end() || view.is_following_end() {
            return None;
        }
        let layout_box = get_scroll_view_box(layout, &view)?;
        Some((indicator, layout_box.clip, get_scrollbar_geometry(layout_box, false).map(|g| g.column)))
    })
    .flatten();
    let Some((indicator, clip, scrollbar_column)) = setup else { return screen };
    if clip.width <= 0 || clip.height <= 0 {
        return screen;
    }
    let row = clip.y + clip.height - 1;
    if row < 0 || row as usize >= screen.len() || is_image_line(&screen[row as usize]) {
        return screen;
    }
    let label = truncate_to_width_with(&indicator(), clip.width as usize, "", false);
    let column = clip.x + (clip.width - visible_width(&label) as i64).div_euclid(2);
    let right_edge = scrollbar_column.unwrap_or(clip.x + clip.width);
    let text = truncate_to_width_with(&label, (right_edge - column).max(0) as usize, "", false);
    let text_width = visible_width(&text);
    if text_width == 0 {
        return screen;
    }
    let mut result = screen;
    let row_index = row as usize;
    result[row_index] = composite_tui_line(&result[row_index], &text, column.max(0) as usize, text_width, width);
    with_alt(inner, |alt| alt.scroll_to_end_indicator_rect = Some((row, column, text_width as i64)));
    result
}

pub(super) fn composite_flashes(inner: &TuiInner, screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
    let lines = with_alt(inner, |alt| alt.flashes.render_lines(width)).unwrap_or_default();
    let lines: Vec<String> = lines.into_iter().rev().take(height).collect::<Vec<_>>().into_iter().rev().collect();
    if lines.is_empty() {
        return screen;
    }
    let mut result = screen;
    if result.len() < height {
        result.resize(height, String::new());
    }
    for (row, line) in lines.iter().enumerate() {
        let flash_width = visible_width(line);
        if flash_width == 0 {
            continue;
        }
        result[row] = composite_tui_line(&result[row], line, width.saturating_sub(flash_width), flash_width, width);
    }
    result
}

fn selection_highlight(text: &str) -> String {
    let mut result = String::from("\x1b[7m");
    let mut index = 0;
    while index < text.len() {
        match extract_ansi_code(text, index) {
            Some(code) => {
                result.push_str(code);
                if code.ends_with('m') {
                    result.push_str("\x1b[7m");
                }
                index += code.len();
            }
            None => {
                let ch = text[index..].chars().next().expect("char boundary");
                result.push(ch);
                index += ch.len_utf8();
            }
        }
    }
    result.push_str("\x1b[27m");
    result
}

/// Draws the selection in reverse video.
pub(super) fn apply_selection(inner: &TuiInner, screen: Vec<String>, layout: Option<&LayoutFrame>) -> Vec<String> {
    let columns = inner.columns() as i64;
    let Some(selection) = selection_bounds(inner) else { return screen };
    let mut on_screen = selection.clone();
    let (mut min_row, mut max_row) = (0i64, screen.len() as i64 - 1);
    let (mut min_col, mut max_col) = (0i64, columns);
    if let Some(view) = &selection.start.scroll_view {
        let Some(layout_box) = layout.and_then(|layout| get_scroll_view_box(layout, view)) else { return screen };
        let (rect, clip) = (layout_box.rect, layout_box.clip);
        min_row = 0.max(rect.y).max(clip.y);
        max_row = (screen.len() as i64 - 1).min(rect.y + rect.height - 1).min(clip.y + clip.height - 1);
        min_col = 0.max(rect.x).max(clip.x);
        max_col = columns.min(rect.x + rect.width).min(clip.x + clip.width);
        let top = view.scroll_top() as i64;
        on_screen.start.row = rect.y + selection.start.row - top;
        on_screen.start.col = rect.x + selection.start.col;
        on_screen.end.row = rect.y + selection.end.row - top;
        on_screen.end.col = rect.x + selection.end.col;
    }
    screen
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            let row = row as i64;
            if row < min_row
                || row > max_row
                || row < on_screen.start.row
                || row > on_screen.end.row
                || is_image_line(&line)
            {
                return line;
            }
            let line_width = visible_width(&line);
            let (start, end) = selection_columns(&line, row, &on_screen, min_col, Some(max_col));
            if end <= start {
                return line;
            }
            let (start, end) = (start as usize, end as usize);
            let before = slice_by_column(&line, 0, start, true);
            let selected = slice_by_column(&line, start, end - start, true);
            let after = slice_by_column(&line, end, line_width.saturating_sub(end), true);
            format!("{before}{}{after}", selection_highlight(&selected))
        })
        .collect()
}
