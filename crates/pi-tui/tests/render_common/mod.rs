//! Shared helpers for the ports of upstream TUI renderer tests (`tui-render.test.ts` and others).
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::env::MapEnv;
use pi_tui::testing::VirtualTerminal;
use pi_tui::{Component, ComponentRef, Terminal, TerminalSink, Tui, TuiOptions, ui_loop};

/// Upstream `TestComponent`: renders whatever `lines` holds.
#[derive(Default)]
pub struct TestComponent {
    pub lines: Vec<String>,
}

impl Component for TestComponent {
    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// A shared [`TestComponent`] with the given lines.
pub fn test_component(lines: &[&str]) -> Rc<RefCell<TestComponent>> {
    Rc::new(RefCell::new(TestComponent { lines: strings(lines) }))
}

pub fn strings(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| line.to_string()).collect()
}

/// `["{prefix}0", "{prefix}1", ...]` with `count` entries.
pub fn numbered(prefix: &str, count: usize) -> Vec<String> {
    (0..count).map(|i| format!("{prefix}{i}")).collect()
}

/// Type-erases a typed component handle.
pub fn erased<C: Component + 'static>(component: &Rc<RefCell<C>>) -> ComponentRef {
    component.clone()
}

/// Options with an empty environment, so the host environment does not leak into the renderer.
pub fn options() -> TuiOptions {
    options_with_env(&[])
}

pub fn options_with_env(vars: &[(&str, &str)]) -> TuiOptions {
    let env = MapEnv::new(vars.iter().map(|(key, value)| (key.to_string(), value.to_string())));
    TuiOptions { env: Some(Rc::new(env)), ..TuiOptions::default() }
}

/// Main-screen TUI on a shared terminal.
pub fn main_tui<T: Terminal + 'static>(terminal: &Rc<RefCell<T>>) -> Tui {
    Tui::new_main_screen(terminal.clone(), options())
}

/// Upstream `waitForRender`: lets the throttled render pipeline settle.
pub fn wait_for_render() {
    ui_loop::run_for(Duration::from_millis(20)).expect("render");
}

/// Whether the viewport cell at `(row, col)` is italic.
pub fn cell_italic(terminal: &VirtualTerminal, row: u16, col: u16) -> bool {
    let cell = terminal.screen().cell(row, col);
    assert!(cell.is_some(), "Missing cell at row {row} col {col}");
    cell.is_some_and(|cell| cell.italic())
}

/// Viewport rows like xterm `translateToString(true)`: only cells never written are trimmed,
/// so written trailing spaces stay.
pub fn xterm_viewport(terminal: &VirtualTerminal) -> Vec<String> {
    let screen = terminal.screen();
    let (rows, cols) = screen.size();
    (0..rows)
        .map(|row| {
            let mut text = String::new();
            let mut kept = 0;
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.has_contents() {
                    text.push_str(cell.contents());
                    kept = text.len();
                } else {
                    text.push(' ');
                }
            }
            text.truncate(kept);
            text
        })
        .collect()
}

/// Resizes like xterm: when the height shrinks above the cursor row, the content scrolls up so
/// the cursor row stays visible. vt100 drops the bottom rows instead.
pub fn xterm_resize(vt: &mut VirtualTerminal, columns: usize, rows: usize) {
    let (col, row) = vt.cursor_position();
    let old_rows = vt.rows();
    if rows < old_rows && row >= rows {
        let scroll = row + 1 - rows;
        let feed = "\n".repeat(scroll);
        vt.write(&format!("\x1b[{old_rows};1H{feed}\x1b[{};{}H", row + 1 - scroll, col + 1));
    }
    vt.resize(columns, rows);
}

/// Upstream `LoggingVirtualTerminal`: logs `write` calls. Cursor visibility and other helper
/// methods go straight to the emulator, as upstream calls xterm directly for them.
pub struct LoggingTerminal {
    pub vt: VirtualTerminal,
    log: Vec<String>,
}

impl LoggingTerminal {
    pub fn shared(columns: usize, rows: usize) -> Rc<RefCell<LoggingTerminal>> {
        Rc::new(RefCell::new(LoggingTerminal { vt: VirtualTerminal::new(columns, rows), log: Vec::new() }))
    }

    pub fn get_writes(&self) -> String {
        self.log.concat()
    }

    pub fn clear_writes(&mut self) {
        self.log.clear();
    }

    pub fn resize(&mut self, columns: usize, rows: usize) {
        xterm_resize(&mut self.vt, columns, rows);
    }
}

impl Terminal for LoggingTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.vt.start(sink);
    }

    fn stop(&mut self) {
        self.vt.stop();
    }

    fn write(&mut self, data: &str) {
        self.log.push(data.to_string());
        self.vt.write(data);
    }

    fn columns(&self) -> usize {
        self.vt.columns()
    }

    fn rows(&self) -> usize {
        self.vt.rows()
    }

    fn kitty_protocol_active(&self) -> bool {
        self.vt.kitty_protocol_active()
    }

    fn move_by(&mut self, lines: i32) {
        self.vt.move_by(lines);
    }

    fn hide_cursor(&mut self) {
        self.vt.hide_cursor();
    }

    fn show_cursor(&mut self) {
        self.vt.show_cursor();
    }

    fn clear_line(&mut self) {
        self.vt.clear_line();
    }

    fn clear_from_cursor(&mut self) {
        self.vt.clear_from_cursor();
    }

    fn clear_screen(&mut self) {
        self.vt.clear_screen();
    }

    fn set_title(&mut self, title: &str) {
        self.vt.set_title(title);
    }
}
