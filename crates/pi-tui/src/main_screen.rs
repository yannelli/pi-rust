//! Main-screen renderer: differential line updates into the terminal scrollback
//! (upstream `TuiMainScreen`).

mod debug;
mod render;
mod support;

use std::collections::BTreeSet;

use crate::debug_log;
use crate::terminal_image::is_image_line;
use crate::tui::{
    Screen, TuiError, TuiInner, TuiStopOptions, extract_cursor_position, normalize_line_resets, resolve_fake_cursors,
};
use crate::utils::visible_width;

/// Renderer state that a TUI taking over the terminal can continue from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TuiMainScreenRenderState {
    pub previous_lines: Vec<String>,
    pub previous_width: i64,
    pub previous_height: i64,
    pub cursor_row: usize,
    pub hardware_cursor_row: usize,
    pub max_lines_rendered: usize,
    pub previous_viewport_top: usize,
}

#[derive(Default)]
pub(crate) struct MainScreenState {
    previous_lines: Vec<String>,
    previous_kitty_image_ids: BTreeSet<u32>,
    /// Zero before the first render, -1 after a forced reset.
    previous_width: i64,
    previous_height: i64,
    cursor_row: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_viewport_top: usize,
}

impl MainScreenState {
    pub fn capture(&self) -> TuiMainScreenRenderState {
        TuiMainScreenRenderState {
            previous_lines: self.previous_lines.clone(),
            previous_width: self.previous_width,
            previous_height: self.previous_height,
            cursor_row: self.cursor_row,
            hardware_cursor_row: self.hardware_cursor_row,
            max_lines_rendered: self.max_lines_rendered,
            previous_viewport_top: self.previous_viewport_top,
        }
    }

    pub fn restore(&mut self, state: TuiMainScreenRenderState) {
        self.previous_lines = state
            .previous_lines
            .into_iter()
            .map(|line| if is_image_line(&line) { String::new() } else { line })
            .collect();
        self.previous_kitty_image_ids.clear();
        self.previous_width = state.previous_width;
        self.previous_height = state.previous_height;
        self.cursor_row = state.cursor_row;
        self.hardware_cursor_row = state.hardware_cursor_row;
        self.max_lines_rendered = state.max_lines_rendered;
        self.previous_viewport_top = state.previous_viewport_top;
    }

    pub fn reset(&mut self) {
        self.previous_lines.clear();
        self.previous_width = -1;
        self.previous_height = -1;
        self.cursor_row = 0;
        self.hardware_cursor_row = 0;
        self.max_lines_rendered = 0;
        self.previous_viewport_top = 0;
    }
}

/// Moves the cursor below the rendered content so the shell prompt starts on a fresh line.
pub(crate) fn before_terminal_stop(inner: &TuiInner, options: TuiStopOptions) {
    let (lines, hardware_row) = match &*inner.screen.borrow() {
        Screen::Main(state) => (state.previous_lines.len(), state.hardware_cursor_row),
        Screen::Alt(_) => return,
    };
    if options.preserve_screen || lines == 0 {
        return;
    }
    let mut terminal = inner.terminal.borrow_mut();
    terminal.write(" ");
    let diff = lines as i64 - hardware_row as i64;
    if diff > 0 {
        terminal.write(&format!("\x1b[{diff}B"));
    } else if diff < 0 {
        terminal.write(&format!("\x1b[{}A", -diff));
    }
    terminal.write("\r\n");
}

/// A line wider than the terminal: the line index and its width.
pub(crate) struct Overflow {
    line: usize,
    width: usize,
}

pub(crate) fn do_render(inner: &TuiInner) -> Result<(), TuiError> {
    if inner.is_stopped() {
        return Ok(());
    }
    let width = inner.columns();
    let height = inner.rows();
    let show_hardware_cursor = inner.state.borrow().show_hardware_cursor;
    let mut new_lines = resolve_fake_cursors(inner.render_root(width), show_hardware_cursor);
    let has_overlays = !inner.state.borrow().overlay_stack.is_empty();
    if has_overlays {
        new_lines = inner.composite_overlays(new_lines, width, height);
    }
    let cursor_pos = extract_cursor_position(&mut new_lines, height);
    normalize_line_resets(&mut new_lines);

    let overflow = {
        let mut screen = inner.screen.borrow_mut();
        let Screen::Main(state) = &mut *screen else { return Ok(()) };
        let mut renderer = render::Renderer { inner, state, width, height, has_overlays, show_hardware_cursor };
        renderer.render(new_lines.clone(), cursor_pos)
    };
    match overflow {
        None => Ok(()),
        Some(overflow) => Err(crash(inner, &new_lines, width, overflow)),
    }
}

fn crash(inner: &TuiInner, lines: &[String], width: usize, overflow: Overflow) -> TuiError {
    let dir = inner.log_directory.clone().unwrap_or_else(|| debug_log::temp_dir(&*inner.env));
    let crash_log = dir.join("pi-tui-crash.log");
    let mut data = vec![
        format!("Crash at {}", debug_log::iso_timestamp()),
        format!("Terminal width: {width}"),
        format!("Line {} visible width: {}", overflow.line, overflow.width),
        String::new(),
        "=== All rendered lines ===".to_string(),
    ];
    data.extend(lines.iter().enumerate().map(|(i, l)| format!("[{i}] (w={}) {l}", visible_width(l))));
    data.push(String::new());
    debug_log::write(&crash_log, &data.join("\n"));
    inner.stop(TuiStopOptions::default());
    TuiError::LineTooWide { line: overflow.line, width: overflow.width, terminal_width: width, crash_log }
}
