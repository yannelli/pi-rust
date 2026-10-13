//! `PI_TUI_DEBUG_REDRAW` and `PI_TUI_DEBUG` logging for the main-screen renderer.

use super::render::Renderer;
use crate::debug_log;

/// Values recorded in a `PI_TUI_DEBUG` render dump.
pub(super) struct DebugDump {
    pub first_changed: usize,
    pub viewport_top: i64,
    pub line_diff: i64,
    pub hardware_cursor_row: i64,
    pub render_end: usize,
    pub final_cursor_row: usize,
    pub cursor_pos: Option<(usize, usize)>,
    pub written_len: usize,
}

impl Renderer<'_> {
    /// Appends a full-redraw reason to `pi-tui-debug.log` when `PI_TUI_DEBUG_REDRAW=1`.
    pub(super) fn log_redraw(&self, reason: &str, new_len: usize) {
        if self.inner.env.var("PI_TUI_DEBUG_REDRAW").as_deref() != Some("1") {
            return;
        }
        let Some(dir) = &self.inner.log_directory else { return };
        let message = format!(
            "[{}] fullRender: {reason} (prev={}, new={new_len}, height={})\n",
            debug_log::iso_timestamp(),
            self.state.previous_lines.len(),
            self.height
        );
        debug_log::append(&dir.join("pi-tui-debug.log"), &message);
    }

    /// Writes one differential render to `<temp>/tui/render-*.log` when `PI_TUI_DEBUG=1`.
    pub(super) fn write_debug_dump(&self, new_lines: &[String], dump: DebugDump) {
        if self.inner.env.var("PI_TUI_DEBUG").as_deref() != Some("1") {
            return;
        }
        let dir = std::env::temp_dir().join("tui");
        let path = dir.join(format!("render-{}-{}.log", debug_log::epoch_millis(), std::process::id()));
        let json = |lines: &[String]| serde_json::to_string_pretty(lines).unwrap_or_default();
        let cursor = match dump.cursor_pos {
            Some((row, col)) => format!("{{\"row\":{row},\"col\":{col}}}"),
            None => "null".to_string(),
        };
        let data = [
            format!("firstChanged: {}", dump.first_changed),
            format!("viewportTop: {}", dump.viewport_top),
            format!("cursorRow: {}", self.state.cursor_row),
            format!("height: {}", self.height),
            format!("lineDiff: {}", dump.line_diff),
            format!("hardwareCursorRow: {}", dump.hardware_cursor_row),
            format!("renderEnd: {}", dump.render_end),
            format!("finalCursorRow: {}", dump.final_cursor_row),
            format!("cursorPos: {cursor}"),
            format!("newLines.length: {}", new_lines.len()),
            format!("previousLines.length: {}", self.state.previous_lines.len()),
            String::new(),
            "=== newLines ===".to_string(),
            json(new_lines),
            String::new(),
            "=== previousLines ===".to_string(),
            json(&self.state.previous_lines),
            String::new(),
            "=== buffer ===".to_string(),
            format!("[{} chars written in bounded chunks]", dump.written_len),
        ]
        .join("\n");
        debug_log::write(&path, &data);
    }
}
