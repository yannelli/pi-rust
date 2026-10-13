//! Helpers shared by the `alt_select_*` ports of the upstream fullscreen renderer tests.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pi_tui::testing::VirtualTerminal;
use pi_tui::ui_loop::{self, TerminalSink};
use pi_tui::{
    Component, CopyCompletion, CopyOutcome, CopyRequest, CopySelectionFn, MouseResponse, Terminal, TerminalRef, Tui,
    TuiAltScreenOptions, TuiMouseEvent, TuiMouseEventResult, TuiOptions, TuiStopOptions, component,
};

/// One call on the [`RecordingTerminal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Start,
    Write(String),
    Stop,
}

/// Port of the upstream `RecordingTerminal`: a virtual terminal that records start, write, and stop.
pub struct RecordingTerminal {
    pub vt: VirtualTerminal,
    pub events: Vec<TerminalEvent>,
}

impl RecordingTerminal {
    pub fn shared(columns: usize, rows: usize) -> Rc<RefCell<RecordingTerminal>> {
        Rc::new(RefCell::new(Self { vt: VirtualTerminal::new(columns, rows), events: Vec::new() }))
    }

    fn record_vt_writes(&mut self, before: usize) {
        let writes: Vec<String> = self.vt.write_calls()[before..].to_vec();
        self.events.extend(writes.into_iter().map(TerminalEvent::Write));
    }
}

impl Terminal for RecordingTerminal {
    fn start(&mut self, sink: TerminalSink) {
        self.events.push(TerminalEvent::Start);
        let before = self.vt.write_calls().len();
        self.vt.start(sink);
        self.record_vt_writes(before);
    }

    fn stop(&mut self) {
        self.events.push(TerminalEvent::Stop);
        let before = self.vt.write_calls().len();
        self.vt.stop();
        self.record_vt_writes(before);
    }

    fn write(&mut self, data: &str) {
        self.events.push(TerminalEvent::Write(data.to_string()));
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
}

/// A fullscreen TUI on a recording terminal.
pub struct Harness {
    pub terminal: Rc<RefCell<RecordingTerminal>>,
    pub tui: Tui,
}

impl Harness {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self::with_options(columns, rows, TuiOptions::default(), TuiAltScreenOptions::default())
    }

    pub fn with_alt(columns: usize, rows: usize, alt: TuiAltScreenOptions) -> Self {
        Self::with_options(columns, rows, TuiOptions::default(), alt)
    }

    pub fn with_options(columns: usize, rows: usize, options: TuiOptions, alt: TuiAltScreenOptions) -> Self {
        let terminal = RecordingTerminal::shared(columns, rows);
        let erased: TerminalRef = terminal.clone();
        let tui = Tui::new_alt_screen(erased, options, alt);
        Self { terminal, tui }
    }

    /// Queues terminal input; the TUI handles it on the next loop pass.
    pub fn send(&self, data: &str) {
        self.terminal.borrow().vt.send_input(data);
    }

    pub fn stop(&self) {
        self.tui.stop(TuiStopOptions::default());
    }

    pub fn event_count(&self) -> usize {
        self.terminal.borrow().events.len()
    }

    pub fn events(&self) -> Vec<TerminalEvent> {
        self.terminal.borrow().events.clone()
    }

    /// Every write recorded after event `start`, concatenated.
    pub fn writes_since(&self, start: usize) -> String {
        self.terminal.borrow().events[start..]
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::Write(data) => Some(data.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn write_count(&self) -> usize {
        self.terminal.borrow().events.iter().filter(|event| matches!(event, TerminalEvent::Write(_))).count()
    }

    /// Number of single writes that contain `needle`.
    pub fn writes_containing(&self, needle: &str) -> usize {
        self.terminal
            .borrow()
            .events
            .iter()
            .filter(|event| matches!(event, TerminalEvent::Write(data) if data.contains(needle)))
            .count()
    }

    pub fn any_write_contains(&self, needle: &str) -> bool {
        self.writes_containing(needle) > 0
    }

    pub fn viewport(&self) -> Vec<String> {
        self.terminal.borrow().vt.viewport()
    }

    /// Visible rows like xterm `translateToString(true)`: only cells never written are trimmed,
    /// so written trailing spaces stay.
    pub fn xterm_viewport(&self) -> Vec<String> {
        let terminal = self.terminal.borrow();
        let screen = terminal.vt.screen();
        let (rows, columns) = screen.size();
        (0..rows)
            .map(|row| {
                let cells: Vec<&vt100::Cell> = (0..columns).filter_map(|col| screen.cell(row, col)).collect();
                let end = cells.iter().rposition(|cell| cell.has_contents()).map_or(0, |index| index + 1);
                cells[..end]
                    .iter()
                    .filter(|cell| !cell.is_wide_continuation())
                    .map(|cell| if cell.has_contents() { cell.contents() } else { " " })
                    .collect()
            })
            .collect()
    }
}

/// Upstream `terminal.waitForRender()`: lets the 20 ms wait and the due renders run.
pub fn wait_for_render() {
    ui_loop::run_for(Duration::from_millis(20)).expect("render");
}

/// Upstream `await request`: runs the UI loop until the copy request has an outcome.
pub fn await_copy(request: CopyRequest) -> bool {
    for _ in 0..200 {
        if let Some(copied) = request.result() {
            return copied;
        }
        ui_loop::run_once(Some(Duration::from_millis(5))).expect("loop");
    }
    panic!("copy request did not complete");
}

/// The OSC 52 sequence that copies `text`.
pub fn osc52(text: &str) -> String {
    use base64::Engine;
    format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(text.as_bytes()))
}

/// Upstream `copySelection: async (text) => { copied.push(text); return true; }`.
pub fn recording_copy(copied: &Rc<RefCell<Vec<String>>>) -> CopySelectionFn {
    let copied = copied.clone();
    Rc::new(move |text: String, completion: CopyCompletion| {
        copied.borrow_mut().push(text);
        completion.complete(CopyOutcome::Copied);
    })
}

/// Upstream copy handlers that resolve to a fixed result.
pub fn fixed_copy(outcome: CopyOutcome) -> CopySelectionFn {
    Rc::new(move |_text: String, completion: CopyCompletion| completion.complete(outcome.clone()))
}

pub type RenderFn = Box<dyn FnMut(usize) -> Vec<String>>;
pub type MouseFn = Box<dyn FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult>>;

/// Upstream `{ render: () => lines, invalidate: () => {} }` with an optional mouse handler.
pub struct FnComponent {
    pub render: RenderFn,
    pub mouse: Option<MouseFn>,
}

impl FnComponent {
    pub fn lines(render: impl FnMut(usize) -> Vec<String> + 'static) -> Self {
        Self { render: Box::new(render), mouse: None }
    }

    pub fn fixed(lines: &[&str]) -> Self {
        let lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        Self::lines(move |_| lines.clone())
    }

    pub fn with_mouse(mut self, mouse: impl FnMut(&TuiMouseEvent) -> Option<TuiMouseEventResult> + 'static) -> Self {
        self.mouse = Some(Box::new(mouse));
        self
    }
}

impl Component for FnComponent {
    fn render(&mut self, width: usize) -> Vec<String> {
        (self.render)(width)
    }

    fn handle_mouse(&mut self, event: &TuiMouseEvent) -> Option<MouseResponse> {
        self.mouse.as_mut().and_then(|mouse| mouse(event)).map(MouseResponse::Handled)
    }
}

/// Port of the upstream `InputOverlay` test component.
#[derive(Default)]
pub struct InputOverlay {
    pub focused: bool,
    pub inputs: Vec<String>,
}

impl Component for InputOverlay {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec!["overlay".to_string()]
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.push(data.to_string());
    }

    fn handles_input(&self) -> bool {
        true
    }

    fn is_focusable(&self) -> bool {
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }
}

pub fn input_overlay() -> Rc<RefCell<InputOverlay>> {
    component(InputOverlay::default())
}

/// `line 1` to `line count`, joined by newlines.
pub fn numbered_lines(count: usize) -> String {
    (1..=count).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n")
}
