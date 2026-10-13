//! The differential render pass of the main-screen renderer (upstream `TuiMainScreen.doRender`).

use std::collections::BTreeSet;

use super::debug::DebugDump;
use super::support::{
    BoundedWriter, collect_kitty_image_ids, delete_kitty_images, expand_changed_range_for_kitty_images,
    extract_kitty_image_ids, kitty_image_reserved_rows,
};
use super::{MainScreenState, Overflow};
use crate::terminal_image::is_image_line;
use crate::tui::TuiInner;
use crate::utils::visible_width;

pub(super) struct Renderer<'a> {
    pub inner: &'a TuiInner,
    pub state: &'a mut MainScreenState,
    pub width: usize,
    pub height: usize,
    pub has_overlays: bool,
    pub show_hardware_cursor: bool,
}

fn move_vertical(output: &mut BoundedWriter<'_>, diff: i64) {
    if diff > 0 {
        output.append(&format!("\x1b[{diff}B"));
    } else if diff < 0 {
        output.append(&format!("\x1b[{}A", -diff));
    }
}

/// Rows between the cursor and a target row, accounting for viewport scrolling.
fn line_diff(target_row: i64, hardware_row: i64, prev_top: i64, top: i64) -> i64 {
    (target_row - top) - (hardware_row - prev_top)
}

impl Renderer<'_> {
    fn writer(&self) -> impl FnMut(&str) + '_ {
        let terminal = &self.inner.terminal;
        move |data: &str| terminal.borrow_mut().write(data)
    }

    fn finish(&mut self, new_lines: Vec<String>) {
        self.state.previous_kitty_image_ids = collect_kitty_image_ids(&new_lines);
        self.state.previous_lines = new_lines;
        self.state.previous_width = self.width as i64;
        self.state.previous_height = self.height as i64;
    }

    fn full_render(&mut self, new_lines: Vec<String>, clear: bool, cursor_pos: Option<(usize, usize)>) {
        self.inner.state.borrow_mut().full_redraw_count += 1;
        let height = self.height;
        let delete = if clear { delete_kitty_images(&self.state.previous_kitty_image_ids) } else { String::new() };
        {
            let mut write = self.writer();
            let mut output = BoundedWriter::new(&mut write);
            output.append("\x1b[?2026h");
            if clear {
                output.append(&delete);
                output.append("\x1b[2J\x1b[H\x1b[3J");
            }
            let mut i = 0;
            while i < new_lines.len() {
                if i > 0 {
                    output.append("\r\n");
                }
                let line = &new_lines[i];
                let reserved = if is_image_line(line) { kitty_image_reserved_rows(&new_lines, i, None) } else { 1 };
                if reserved > 1 && reserved <= height {
                    for _ in 1..reserved {
                        output.append("\r\n");
                    }
                    output.append(&format!("\x1b[{}A", reserved - 1));
                    output.append(line);
                    output.append(&format!("\x1b[{}B", reserved - 1));
                    i += reserved;
                    continue;
                }
                output.append(line);
                i += 1;
            }
            output.append("\x1b[?2026l");
            output.flush();
        }
        let len = new_lines.len();
        self.state.cursor_row = len.saturating_sub(1);
        self.state.hardware_cursor_row = self.state.cursor_row;
        self.state.max_lines_rendered = if clear { len } else { self.state.max_lines_rendered.max(len) };
        self.state.previous_viewport_top = height.max(len) - height;
        self.position_hardware_cursor(cursor_pos, len);
        self.finish(new_lines);
    }

    /// Renders one frame. Returns the first line that is wider than the terminal, if any.
    pub fn render(&mut self, new_lines: Vec<String>, cursor_pos: Option<(usize, usize)>) -> Option<Overflow> {
        let (width, height) = (self.width as i64, self.height as i64);
        let width_changed = self.state.previous_width != 0 && self.state.previous_width != width;
        let height_changed = self.state.previous_height != 0 && self.state.previous_height != height;
        let previous_buffer_length = if self.state.previous_height > 0 {
            self.state.previous_viewport_top as i64 + self.state.previous_height
        } else {
            height
        };
        let mut prev_viewport_top = if height_changed {
            (previous_buffer_length - height).max(0)
        } else {
            self.state.previous_viewport_top as i64
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.state.hardware_cursor_row as i64;
        let previous_len = self.state.previous_lines.len();
        let new_len = new_lines.len();

        if previous_len == 0 && !width_changed && !height_changed {
            self.log_redraw("first render", new_len);
            self.full_render(new_lines, false, cursor_pos);
            return None;
        }
        if width_changed {
            self.log_redraw(&format!("terminal width changed ({} -> {width})", self.state.previous_width), new_len);
            self.full_render(new_lines, true, cursor_pos);
            return None;
        }
        // Termux changes the height when the software keyboard shows or hides; a full redraw
        // there would replay the whole history on every toggle.
        if height_changed && self.inner.env.non_empty("TERMUX_VERSION").is_none() {
            self.log_redraw(&format!("terminal height changed ({} -> {height})", self.state.previous_height), new_len);
            self.full_render(new_lines, true, cursor_pos);
            return None;
        }
        let clear_on_shrink = self.inner.state.borrow().clear_on_shrink;
        if clear_on_shrink && new_len < self.state.max_lines_rendered && !self.has_overlays {
            self.log_redraw(&format!("clearOnShrink (maxLinesRendered={})", self.state.max_lines_rendered), new_len);
            self.full_render(new_lines, true, cursor_pos);
            return None;
        }

        let mut first_changed: Option<usize> = None;
        let mut last_changed = 0usize;
        for i in 0..new_len.max(previous_len) {
            let old = self.state.previous_lines.get(i).map(String::as_str).unwrap_or("");
            let new = new_lines.get(i).map(String::as_str).unwrap_or("");
            if old != new {
                first_changed.get_or_insert(i);
                last_changed = i;
            }
        }
        let appended = new_len > previous_len;
        if appended {
            first_changed.get_or_insert(previous_len);
            last_changed = new_len - 1;
        }
        let Some(first_changed) = first_changed else {
            self.position_hardware_cursor(cursor_pos, new_len);
            self.state.previous_viewport_top = prev_viewport_top as usize;
            self.state.previous_height = height;
            return None;
        };
        let (first_changed, last_changed) =
            expand_changed_range_for_kitty_images(first_changed, last_changed, &self.state.previous_lines, &new_lines);
        let append_start = appended && first_changed == previous_len && first_changed > 0;

        if first_changed >= new_len {
            if previous_len > new_len {
                let target_row = new_len.saturating_sub(1) as i64;
                if target_row < prev_viewport_top {
                    let reason = format!("deleted lines moved viewport up ({target_row} < {prev_viewport_top})");
                    self.log_redraw(&reason, new_len);
                    self.full_render(new_lines, true, cursor_pos);
                    return None;
                }
                let extra_lines = previous_len - new_len;
                if extra_lines as i64 > height {
                    self.log_redraw(&format!("extraLines > height ({extra_lines} > {height})"), new_len);
                    self.full_render(new_lines, true, cursor_pos);
                    return None;
                }
                let delete = self.delete_changed_kitty_images(first_changed, last_changed);
                {
                    let mut write = self.writer();
                    let mut output = BoundedWriter::new(&mut write);
                    output.append("\x1b[?2026h");
                    output.append(&delete);
                    move_vertical(
                        &mut output,
                        line_diff(target_row, hardware_cursor_row, prev_viewport_top, viewport_top),
                    );
                    output.append("\r");
                    let clear_start_offset = if new_len == 0 { 0 } else { 1 };
                    if extra_lines > 0 && clear_start_offset > 0 {
                        output.append(&format!("\x1b[{clear_start_offset}B"));
                    }
                    for i in 0..extra_lines {
                        output.append("\r\x1b[2K");
                        if i + 1 < extra_lines {
                            output.append("\x1b[1B");
                        }
                    }
                    let move_back = (extra_lines + clear_start_offset).saturating_sub(1);
                    if move_back > 0 {
                        output.append(&format!("\x1b[{move_back}A"));
                    }
                    output.append("\x1b[?2026l");
                    output.flush();
                }
                self.state.cursor_row = target_row as usize;
                self.state.hardware_cursor_row = target_row as usize;
            }
            self.position_hardware_cursor(cursor_pos, new_len);
            self.finish(new_lines);
            self.state.previous_viewport_top = prev_viewport_top as usize;
            return None;
        }

        if (first_changed as i64) < prev_viewport_top {
            self.log_redraw(&format!("firstChanged < viewportTop ({first_changed} < {prev_viewport_top})"), new_len);
            self.full_render(new_lines, true, cursor_pos);
            return None;
        }

        let delete = self.delete_changed_kitty_images(first_changed, last_changed);
        let render_end = last_changed.min(new_len - 1);
        let mut final_cursor_row = render_end;
        let debug_line_diff;
        let mut written_len = 0;
        let mut overflow = None;
        let mut needs_full_render: Option<String> = None;
        {
            let mut write = self.writer();
            let mut output = BoundedWriter::new(&mut write);
            output.append("\x1b[?2026h");
            output.append(&delete);
            let prev_viewport_bottom = prev_viewport_top + height - 1;
            let move_target_row = if append_start { first_changed as i64 - 1 } else { first_changed as i64 };
            if move_target_row > prev_viewport_bottom {
                let current_screen_row = (hardware_cursor_row - prev_viewport_top).clamp(0, (height - 1).max(0));
                let move_to_bottom = height - 1 - current_screen_row;
                if move_to_bottom > 0 {
                    output.append(&format!("\x1b[{move_to_bottom}B"));
                }
                let scroll = move_target_row - prev_viewport_bottom;
                output.append(&"\r\n".repeat(scroll as usize));
                prev_viewport_top += scroll;
                viewport_top += scroll;
                hardware_cursor_row = move_target_row;
            }
            debug_line_diff = line_diff(move_target_row, hardware_cursor_row, prev_viewport_top, viewport_top);
            move_vertical(&mut output, debug_line_diff);
            output.append(if append_start { "\r\n" } else { "\r" });

            let mut i = first_changed;
            while i <= render_end {
                if i > first_changed {
                    output.append("\r\n");
                }
                let line = &new_lines[i];
                let is_image = is_image_line(line);
                let reserved = if is_image { kitty_image_reserved_rows(&new_lines, i, Some(render_end)) } else { 1 };
                if reserved > 1 {
                    let start_row = i as i64 - viewport_top;
                    if start_row < 0 || start_row + reserved as i64 > height {
                        needs_full_render =
                            Some(format!("kitty image pre-clear would scroll ({start_row} + {reserved} > {height})"));
                        break;
                    }
                    output.append("\x1b[2K");
                    for _ in 1..reserved {
                        output.append("\r\n\x1b[2K");
                    }
                    output.append(&format!("\x1b[{}A", reserved - 1));
                    output.append(line);
                    output.append(&format!("\x1b[{}B", reserved - 1));
                    i += reserved;
                    continue;
                }
                output.append("\x1b[2K");
                let line_width = visible_width(line);
                if !is_image && line_width > self.width {
                    overflow = Some(Overflow { line: i, width: line_width });
                    break;
                }
                output.append(line);
                i += 1;
            }
            if needs_full_render.is_none() && overflow.is_none() {
                if previous_len > new_len {
                    if render_end + 1 < new_len {
                        output.append(&format!("\x1b[{}B", new_len - 1 - render_end));
                        final_cursor_row = new_len - 1;
                    }
                    for _ in new_len..previous_len {
                        output.append("\r\n\x1b[2K");
                    }
                    output.append(&format!("\x1b[{}A", previous_len - new_len));
                }
                output.append("\x1b[?2026l");
                written_len = output.len();
                output.flush();
            }
        }
        if let Some(reason) = needs_full_render {
            self.log_redraw(&reason, new_len);
            self.full_render(new_lines, true, cursor_pos);
            return None;
        }
        if overflow.is_some() {
            return overflow;
        }
        self.write_debug_dump(
            &new_lines,
            DebugDump {
                first_changed,
                viewport_top,
                line_diff: debug_line_diff,
                hardware_cursor_row,
                render_end,
                final_cursor_row,
                cursor_pos,
                written_len,
            },
        );

        self.state.cursor_row = new_len.saturating_sub(1);
        self.state.hardware_cursor_row = final_cursor_row;
        self.state.max_lines_rendered = self.state.max_lines_rendered.max(new_len);
        self.state.previous_viewport_top = prev_viewport_top.max(final_cursor_row as i64 - height + 1).max(0) as usize;
        self.position_hardware_cursor(cursor_pos, new_len);
        self.finish(new_lines);
        None
    }

    fn delete_changed_kitty_images(&self, first_changed: usize, last_changed: usize) -> String {
        if last_changed < first_changed || self.state.previous_lines.is_empty() {
            return String::new();
        }
        let max_line = last_changed.min(self.state.previous_lines.len() - 1);
        let mut ids = BTreeSet::new();
        for line in self.state.previous_lines.iter().take(max_line + 1).skip(first_changed) {
            ids.extend(extract_kitty_image_ids(line));
        }
        delete_kitty_images(&ids)
    }

    /// Positions the hardware cursor for IME candidate windows.
    fn position_hardware_cursor(&mut self, cursor_pos: Option<(usize, usize)>, total_lines: usize) {
        let Some((row, col)) = cursor_pos.filter(|_| total_lines > 0) else {
            self.inner.terminal.borrow_mut().hide_cursor();
            return;
        };
        let target_row = row.min(total_lines - 1);
        let delta = target_row as i64 - self.state.hardware_cursor_row as i64;
        let mut buffer = String::new();
        if delta > 0 {
            buffer.push_str(&format!("\x1b[{delta}B"));
        } else if delta < 0 {
            buffer.push_str(&format!("\x1b[{}A", -delta));
        }
        buffer.push_str(&format!("\x1b[{}G", col + 1));
        let mut terminal = self.inner.terminal.borrow_mut();
        terminal.write(&buffer);
        self.state.hardware_cursor_row = target_row;
        if self.show_hardware_cursor {
            terminal.show_cursor();
        } else {
            terminal.hide_cursor();
        }
    }
}
