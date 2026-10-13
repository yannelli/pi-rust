//! Line-level helpers: overlay compositing, line resets, cursor marker extraction, fake cursors.

use std::sync::LazyLock;

use regex::Regex;

use crate::component::{CURSOR_MARKER, FAKE_CURSOR_END, FAKE_CURSOR_START};
use crate::terminal_image::is_image_line;
use crate::utils::{extract_segments, normalize_terminal_output, slice_by_column, slice_with_width, visible_width};

/// Resets SGR attributes and closes any open OSC 8 hyperlink.
pub(crate) const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

static FOCUSED_FAKE_CURSOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)(\x1b_pi:c\x07)\x1b_pi:fc\x07(.*?)(?:\x1b_pi:/fc\x07|$)").expect("valid regex"));

/// Composites overlay content into a terminal line at a fixed column.
pub fn composite_tui_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    if is_image_line(base_line) {
        return base_line.to_string();
    }
    let after_start = start_col + overlay_width;
    let base = extract_segments(base_line, start_col, after_start, total_width.saturating_sub(after_start), true);
    let overlay = slice_with_width(overlay_line, 0, overlay_width, true);
    let before_pad = start_col.saturating_sub(base.before_width);
    let overlay_pad = overlay_width.saturating_sub(overlay.width);
    let actual_before_width = start_col.max(base.before_width);
    let actual_overlay_width = overlay_width.max(overlay.width);
    let after_target = total_width.saturating_sub(actual_before_width + actual_overlay_width);
    let after_pad = after_target.saturating_sub(base.after_width);
    let mut result = String::with_capacity(base_line.len() + overlay_line.len() + 32);
    result.push_str(&base.before);
    result.push_str(&" ".repeat(before_pad));
    result.push_str(SEGMENT_RESET);
    result.push_str(&overlay.text);
    result.push_str(&" ".repeat(overlay_pad));
    result.push_str(SEGMENT_RESET);
    result.push_str(&base.after);
    result.push_str(&" ".repeat(after_pad));
    if visible_width(&result) <= total_width { result } else { slice_by_column(&result, 0, total_width, true) }
}

/// Normalizes each non-image line and appends a full reset so styles never leak to the next row
/// (upstream `applyLineResets`).
pub fn normalize_line_resets(lines: &mut [String]) {
    for line in lines.iter_mut() {
        if !is_image_line(line) {
            let mut normalized = normalize_terminal_output(line);
            normalized.push_str(SEGMENT_RESET);
            *line = normalized;
        }
    }
}

/// Finds the cursor marker in the bottom `height` lines, strips it, and returns `(row, col)`.
pub fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let viewport_top = lines.len().saturating_sub(height);
    for row in (viewport_top..lines.len()).rev() {
        if let Some(index) = lines[row].find(CURSOR_MARKER) {
            let col = visible_width(&lines[row][..index]);
            lines[row].replace_range(index..index + CURSOR_MARKER.len(), "");
            return Some((row, col));
        }
    }
    None
}

/// Turns fake cursor markers into reverse video. With the hardware cursor shown, a fake cursor
/// right after the cursor marker is dropped because the terminal draws the cursor there.
pub fn resolve_fake_cursors(lines: Vec<String>, show_hardware_cursor: bool) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            if !line.contains(FAKE_CURSOR_START) {
                return line;
            }
            let resolved =
                if show_hardware_cursor { FOCUSED_FAKE_CURSOR.replace(&line, "${1}${2}").into_owned() } else { line };
            resolved.replace(FAKE_CURSOR_START, "\x1b[7m").replace(FAKE_CURSOR_END, "\x1b[27m")
        })
        .collect()
}
