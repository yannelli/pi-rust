//! Port of the `TUI.queryTerminalColors` cases in upstream `test/terminal-colors.test.ts`.
//! The parser cases live in `colors_terminal_colors.rs`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::terminal_colors::{RgbColor, TerminalColors};
use pi_tui::ui_loop::{self, TerminalSink};
use pi_tui::{Component, Terminal, Tui, TuiOptions, TuiStopOptions, component};

/// Upstream `TestTerminal`: records writes; cursor and screen helpers write nothing.
#[derive(Default)]
struct TestTerminal {
    sink: Option<TerminalSink>,
    writes: Vec<String>,
}

impl Terminal for TestTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.sink = Some(sink);
    }

    fn stop(&mut self) {
        self.sink = None;
    }

    fn write(&mut self, data: &str) {
        self.writes.push(data.to_string());
    }

    fn columns(&self) -> usize {
        80
    }

    fn rows(&self) -> usize {
        24
    }

    fn kitty_protocol_active(&self) -> bool {
        false
    }

    fn move_by(&mut self, _lines: i32) {}
    fn hide_cursor(&mut self) {}
    fn show_cursor(&mut self) {}
    fn clear_line(&mut self) {}
    fn clear_from_cursor(&mut self) {}
    fn clear_screen(&mut self) {}
    fn set_title(&mut self, _title: &str) {}
}

#[derive(Default)]
struct InputRecorder {
    inputs: Vec<String>,
}

impl Component for InputRecorder {
    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
    }

    fn handles_input(&self) -> bool {
        true
    }
}

struct Setup {
    terminal: Rc<RefCell<TestTerminal>>,
    tui: Tui,
    component: Rc<RefCell<InputRecorder>>,
}

impl Setup {
    fn new() -> Self {
        let terminal = Rc::new(RefCell::new(TestTerminal::default()));
        let tui = Tui::new_main_screen(terminal.clone(), TuiOptions::default());
        let component = component(InputRecorder::default());
        tui.add_child(component.clone());
        tui.set_focus(Some(component.clone()));
        tui.start();
        Self { terminal, tui, component }
    }

    /// Upstream `terminal.sendInput(data)`: the input reaches the TUI through the terminal sink.
    fn send_input(&self, data: &str) {
        let sink = self.terminal.borrow().sink.clone().expect("terminal started");
        sink.input(data);
        ui_loop::run_pending().unwrap();
    }

    fn last_write(&self) -> String {
        self.terminal.borrow().writes.last().cloned().unwrap_or_default()
    }

    fn inputs(&self) -> Vec<String> {
        self.component.borrow().inputs.clone()
    }

    /// Starts a query and returns the cell its result lands in.
    fn query(&self, timeout_ms: u64, on_late_reply: Option<Box<dyn FnOnce(TerminalColors)>>) -> ColorsCell {
        let result: ColorsCell = Rc::new(RefCell::new(None));
        let slot = result.clone();
        self.tui.query_terminal_colors(
            Duration::from_millis(timeout_ms),
            move |colors| *slot.borrow_mut() = Some(colors),
            on_late_reply,
        );
        result
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.tui.stop(TuiStopOptions::default());
    }
}

type ColorsCell = Rc<RefCell<Option<TerminalColors>>>;

const DA1: &str = "\x1b[?62;22c";
const BLACK: RgbColor = RgbColor { r: 0, g: 0, b: 0 };
const WHITE: RgbColor = RgbColor { r: 255, g: 255, b: 255 };

fn palette_replies() -> Vec<String> {
    (0..16).map(|index| format!("\x1b]4;{index};#000000\x07")).collect()
}

#[test]
fn queries_all_colors_in_one_write_and_consumes_the_replies() {
    let setup = Setup::new();
    let query = setup.query(1000, None);
    let written = setup.last_write();
    assert!(written.starts_with("\x1b]10;?\x07\x1b]11;?\x07\x1b]4;0;?\x07") && written.ends_with("\x1b[c"));

    setup.send_input("x");
    setup.send_input("\x1b]10;#ffffff\x07");
    setup.send_input("\x1b]11;rgb:0000/0000/0000\x1b\\");
    for reply in palette_replies() {
        setup.send_input(&reply);
    }
    // Resolves once every reply arrived, without waiting for DA1.
    assert_eq!(
        *query.borrow(),
        Some(TerminalColors { foreground: Some(WHITE), background: Some(BLACK), palette: Some(vec![BLACK; 16]) })
    );
    setup.send_input(DA1);
    assert_eq!(setup.inputs(), ["x"]);
}

#[test]
fn resolves_on_da1_with_the_replies_that_arrived_in_query_order() {
    let setup = Setup::new();
    let first = setup.query(1000, None);
    let second = setup.query(1000, None);
    setup.send_input("\x1b]11;#000000\x07");
    // An incomplete palette is dropped.
    for reply in &palette_replies()[..8] {
        setup.send_input(reply);
    }
    setup.send_input(DA1);
    setup.send_input(DA1);

    assert_eq!(*first.borrow(), Some(TerminalColors { foreground: None, background: Some(BLACK), palette: None }));
    assert_eq!(*second.borrow(), Some(TerminalColors::default()));
}

#[test]
fn reports_late_replies_after_a_timeout_and_consumes_them_until_da1() {
    let setup = Setup::new();
    let late: Rc<RefCell<Vec<TerminalColors>>> = Rc::new(RefCell::new(Vec::new()));
    let late_sink = late.clone();
    let query = setup.query(1, Some(Box::new(move |colors| late_sink.borrow_mut().push(colors))));
    ui_loop::run_for(Duration::from_millis(5)).unwrap();
    let resolved = query.borrow().clone().expect("resolved at the timeout");
    assert_eq!(resolved.background, None);

    setup.send_input("\x1b]11;#ffffff\x07");
    setup.send_input(DA1);
    assert_eq!(*late.borrow(), [TerminalColors { foreground: None, background: Some(WHITE), palette: None }]);
    assert!(setup.inputs().is_empty());

    // With no query pending, color replies are ordinary input again.
    setup.send_input("\x1b]11;#ffffff\x07");
    assert_eq!(setup.inputs(), ["\x1b]11;#ffffff\x07"]);
}
