//! Painting laid-out boxes and scrollbars into screen rows.

use std::sync::LazyLock;

use regex::Regex;

use super::{LayoutBox, OSC133_ZONE_PREFIX};
use crate::components::scroll_view::ScrollViewScrollbar;
use crate::terminal_image::{crop_kitty_image_line, get_kitty_image_metadata, is_image_line};
use crate::tui::composite_tui_line;
use crate::utils::{
    extract_ansi_code, get_active_background_ansi, get_grapheme_cell_range, js_trim, slice_by_column, visible_width,
};

static SGR_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;:]*m").expect("valid regex"));

/// Position of a scroll view's scrollbar track and thumb, in screen cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: i64,
    pub track_top: i64,
    pub track_height: i64,
    pub thumb_top: i64,
    pub thumb_height: i64,
    pub max_scroll_top: i64,
}

fn replace_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    replacement: &str,
    preserve_target_background: bool,
) -> String {
    if is_image_line(line) {
        return line.to_string();
    }
    let range = get_grapheme_cell_range(line, column);
    let start = range.as_ref().map_or(column, |r| r.start);
    let end = range.as_ref().map_or(column + 1, |r| r.end);
    let before = slice_by_column(line, 0, start, true);
    let target = slice_by_column(line, start, end - start, true);
    let after = slice_by_column(line, end, total_width.saturating_sub(end), true);

    let mut target_prefix = String::new();
    let mut index = 0;
    while index < target.len() {
        let Some(code) = extract_ansi_code(&target, index) else { break };
        target_prefix.push_str(code);
        index += code.len();
    }
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    let cell_padding_before = " ".repeat(column.saturating_sub(start));
    let cell_padding_after = " ".repeat(end.saturating_sub(column + 1));
    let background =
        if preserve_target_background { get_active_background_ansi(&target_prefix) } else { String::new() };
    format!(
        "{before}{before_padding}\x1b[0m\x1b]8;;\x07{background}{cell_padding_before}{replacement}{cell_padding_after}{after}"
    )
}

/// Scrollbar geometry of a scroll view box, if its scrollbar is shown. With `include_hidden_auto`,
/// an `Auto` scrollbar that is currently hidden but could appear is included too.
pub fn get_scrollbar_geometry(layout_box: &LayoutBox, include_hidden_auto: bool) -> Option<ScrollbarGeometry> {
    let scroll_view = layout_box.scroll_view.as_ref()?;
    if layout_box.rect.width <= 0 || layout_box.rect.height <= 0 {
        return None;
    }
    let content_height = layout_box
        .children
        .first()
        .map(|c| c.rect.height)
        .or_else(|| layout_box.scroll_content_lines.as_ref().map(|l| l.len() as i64))
        .unwrap_or(0);
    let track_height = layout_box.rect.height;
    let can_reveal =
        include_hidden_auto && scroll_view.scrollbar() == ScrollViewScrollbar::Auto && content_height > track_height;
    if !scroll_view.is_scrollbar_visible() && !can_reveal {
        return None;
    }
    let min_thumb = 2.min(track_height);
    let proportional = ((track_height * track_height) as f64 / content_height.max(1) as f64).round() as i64;
    let thumb_height = proportional.min(track_height).max(min_thumb);
    let max_scroll_top = (content_height - track_height).max(0);
    let max_thumb_top = track_height - thumb_height;
    let thumb_offset = if max_scroll_top == 0 {
        0
    } else {
        ((scroll_view.scroll_top() as f64 / max_scroll_top as f64) * max_thumb_top as f64).round() as i64
    };
    let column = layout_box.rect.x + layout_box.rect.width - 1;
    if column < layout_box.clip.x || column >= layout_box.clip.x + layout_box.clip.width {
        return None;
    }
    Some(ScrollbarGeometry {
        column,
        track_top: layout_box.rect.y,
        track_height,
        thumb_top: layout_box.rect.y + thumb_offset,
        thumb_height,
        max_scroll_top,
    })
}

fn paint_scrollbar(layout_box: &LayoutBox, screen: &mut [String], total_width: usize) {
    let Some(geometry) = get_scrollbar_geometry(layout_box, false) else { return };
    let Some(scroll_view) = &layout_box.scroll_view else { return };
    for offset in 0..geometry.track_height {
        let row = geometry.track_top + offset;
        let clip = layout_box.clip;
        if row < clip.y || row >= clip.y + clip.height || row < 0 || row >= screen.len() as i64 {
            continue;
        }
        let is_thumb = row >= geometry.thumb_top && row < geometry.thumb_top + geometry.thumb_height;
        let replacement = if is_thumb {
            scroll_view.scrollbar_thumb_style(if scroll_view.is_scrollbar_active() { "█" } else { "┃" })
        } else {
            scroll_view.scrollbar_track_style("│")
        };
        let row = row as usize;
        screen[row] = replace_scrollbar_cell(
            &screen[row],
            geometry.column.max(0) as usize,
            total_width,
            &replacement,
            scroll_view.scrollbar() != ScrollViewScrollbar::Always,
        );
    }
}

pub(super) fn paint_box(layout_box: &LayoutBox, screen: &mut [String], total_width: usize) {
    let rect = layout_box.rect;
    let clip = layout_box.clip;
    if let Some(lines) = &layout_box.lines {
        let first_row = rect.y.max(clip.y).max(0);
        let last_row = (rect.y + rect.height).min(clip.y + clip.height).min(screen.len() as i64);
        for row in first_row..last_row {
            let index = layout_box.line_offset as i64 + row - rect.y;
            let Some(source) = usize::try_from(index).ok().and_then(|i| lines.get(i)) else { continue };
            let mut line = OSC133_ZONE_PREFIX.replace(source, "").into_owned();
            if let Some(metadata) = get_kitty_image_metadata(&line) {
                let clip_bottom = (screen.len() as i64).min(clip.y + clip.height);
                let visible_rows = (metadata.rows as i64).min(clip_bottom - row);
                if visible_rows < metadata.rows as i64 {
                    line = crop_kitty_image_line(&line, 0, visible_rows.max(0) as usize);
                }
            }
            let row = row as usize;
            // A full-width box on an untouched row can use the line as is; rows are written with
            // erase-line and the final width clamp still truncates wide lines.
            if rect.x == 0 && rect.width >= total_width as i64 && (is_image_line(&line) || screen[row].is_empty()) {
                screen[row] = line;
            } else {
                screen[row] = composite_tui_line(
                    &screen[row],
                    &line,
                    rect.x.max(0) as usize,
                    rect.width.max(0) as usize,
                    total_width,
                );
            }
        }
    }
    for child in &layout_box.children {
        paint_box(child, screen, total_width);
    }

    if let (Some(scroll_view), Some(content)) = (&layout_box.scroll_view, &layout_box.scroll_content_lines) {
        let scroll_top = scroll_view.scroll_top();
        if scroll_top > 0 && rect.height > 0 {
            for image_row in (0..scroll_top).rev() {
                let image_line = content.get(image_row).map(String::as_str).unwrap_or("");
                if let Some(metadata) = get_kitty_image_metadata(image_line) {
                    let hidden_rows = scroll_top - image_row;
                    if hidden_rows < metadata.rows {
                        let visible_rows = (rect.height as usize).min(metadata.rows - hidden_rows);
                        let cropped = crop_kitty_image_line(image_line, hidden_rows, visible_rows);
                        if rect.x == 0 && rect.width >= total_width as i64 && rect.y >= 0 {
                            if let Some(slot) = screen.get_mut(rect.y as usize) {
                                *slot = cropped;
                            }
                        }
                    }
                    break;
                }
                // Image placeholder rows inside padded or colored boxes contain only
                // spaces and SGR codes; keep searching past them for the image line.
                if !js_trim(&SGR_CODE.replace_all(image_line, "")).is_empty() {
                    break;
                }
            }
        }
    }

    paint_scrollbar(layout_box, screen, total_width);
}
