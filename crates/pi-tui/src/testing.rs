//! [`VirtualTerminal`]: an in-memory terminal emulator for tests of components and renderers.

use std::cell::RefCell;
use std::rc::Rc;

use crate::terminal::Terminal;
use crate::ui_loop::TerminalSink;

const SCROLLBACK: usize = 10_000;

/// A terminal backed by the `vt100` emulator. Output is parsed like a real terminal; input sent
/// with [`VirtualTerminal::send_input`] goes to the UI loop, so call
/// [`crate::ui_loop::run_pending`] afterwards.
pub struct VirtualTerminal {
    parser: vt100::Parser,
    columns: usize,
    rows: usize,
    sink: Option<TerminalSink>,
    writes: Vec<String>,
    kitty_protocol_active: bool,
}

impl VirtualTerminal {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self {
            parser: vt100::Parser::new(rows as u16, columns as u16, SCROLLBACK),
            columns,
            rows,
            sink: None,
            writes: Vec::new(),
            kitty_protocol_active: true,
        }
    }

    /// A shared handle that coerces to [`crate::terminal::TerminalRef`].
    pub fn shared(columns: usize, rows: usize) -> Rc<RefCell<VirtualTerminal>> {
        Rc::new(RefCell::new(Self::new(columns, rows)))
    }

    /// Reports the Kitty keyboard protocol as active or not (default active, like upstream tests).
    pub fn set_kitty_protocol_active(&mut self, active: bool) {
        self.kitty_protocol_active = active;
    }

    /// Simulates keyboard input. The TUI receives it on the next UI loop pass.
    pub fn send_input(&self, data: &str) {
        if let Some(sink) = &self.sink {
            sink.input(data);
        }
    }

    /// Resizes the terminal and notifies the TUI.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        self.columns = columns;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows as u16, columns as u16);
        if let Some(sink) = &self.sink {
            sink.resize();
        }
    }

    /// Visible rows with trailing spaces removed.
    pub fn viewport(&self) -> Vec<String> {
        self.parser.screen().rows(0, self.columns as u16).map(|row| row.trim_end().to_string()).collect()
    }

    /// Scrollback followed by the visible rows.
    pub fn scroll_buffer(&mut self) -> Vec<String> {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let total = screen.scrollback();
        let mut lines = Vec::with_capacity(total + self.rows);
        for offset in (1..=total).rev() {
            screen.set_scrollback(offset);
            let top = screen.rows(0, self.columns as u16).next().unwrap_or_default();
            lines.push(top.trim_end().to_string());
        }
        screen.set_scrollback(0);
        lines.extend(self.viewport());
        lines
    }

    /// Cursor position as `(x, y)`.
    pub fn cursor_position(&self) -> (usize, usize) {
        let (row, col) = self.parser.screen().cursor_position();
        (col as usize, row as usize)
    }

    /// Whether the cursor is hidden (DECTCEM).
    pub fn cursor_hidden(&self) -> bool {
        self.parser.screen().hide_cursor()
    }

    /// The emulator screen, for cell attributes and modes.
    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Everything written so far, concatenated.
    pub fn writes(&self) -> String {
        self.writes.concat()
    }

    /// Individual write calls so far.
    pub fn write_calls(&self) -> &[String] {
        &self.writes
    }

    pub fn clear_writes(&mut self) {
        self.writes.clear();
    }

    /// Resets the emulator to an empty screen.
    pub fn reset(&mut self) {
        self.parser = vt100::Parser::new(self.rows as u16, self.columns as u16, SCROLLBACK);
    }
}

impl Terminal for VirtualTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.sink = Some(sink);
        self.write("\x1b[?2004h");
    }

    fn stop(&mut self) {
        self.write("\x1b[?2004l");
        self.sink = None;
    }

    fn write(&mut self, data: &str) {
        self.writes.push(data.to_string());
        self.parser.process(data.as_bytes());
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }
}
