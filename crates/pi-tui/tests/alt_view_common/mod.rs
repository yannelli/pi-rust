//! Helpers shared by the fullscreen (`TuiAltScreen`) test files.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::MutexGuard;
use std::time::Duration;

use pi_tui::env::{EnvLookup, MapEnv};
use pi_tui::testing::VirtualTerminal;
use pi_tui::{
    Component, ComponentRef, ScrollView, ScrollViewHandle, ScrollViewOptions, Tui, TuiAltScreenOptions, TuiOptions,
    TuiStopOptions, component,
};

/// A fullscreen TUI on a virtual terminal. Holds the global state lock, because the renderer
/// reads process-wide keybindings and image capabilities.
pub struct Harness {
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub tui: Tui,
    _guard: MutexGuard<'static, ()>,
}

/// Environment of a plain terminal outside any multiplexer.
pub fn plain_env() -> Rc<dyn EnvLookup> {
    Rc::new(MapEnv::new([("TERM", "xterm-256color")]))
}

impl Harness {
    pub fn new(columns: usize, rows: usize) -> Self {
        Self::with_options(columns, rows, TuiAltScreenOptions::default())
    }

    pub fn with_options(columns: usize, rows: usize, options: TuiAltScreenOptions) -> Self {
        Self::with_env(columns, rows, plain_env(), options)
    }

    pub fn with_env(columns: usize, rows: usize, env: Rc<dyn EnvLookup>, options: TuiAltScreenOptions) -> Self {
        let guard = pi_tui::env::global_state_lock();
        let terminal = VirtualTerminal::shared(columns, rows);
        let tui_options = TuiOptions { env: Some(env), ..TuiOptions::default() };
        let tui = Tui::new_alt_screen(terminal.clone(), tui_options, options);
        Self { terminal, tui, _guard: guard }
    }

    /// Upstream `terminal.sendInput(data)`, which reaches the TUI synchronously.
    pub fn send(&self, data: &str) {
        self.tui.handle_terminal_input(data);
    }

    /// Upstream `terminal.waitForRender()`: lets the 16 ms render throttle and timers run.
    pub fn wait(&self) {
        wait_ms(20);
    }

    /// Upstream `getViewport().map(trimEnd)`.
    pub fn viewport(&self) -> Vec<String> {
        self.terminal.borrow().viewport()
    }

    /// Upstream `getViewport()`: written spaces are kept, never written cells are not.
    pub fn raw_viewport(&self) -> Vec<String> {
        let terminal = self.terminal.borrow();
        let columns = terminal.screen().size().1;
        terminal.screen().rows(0, columns).collect()
    }

    pub fn viewport_has(&self, needle: &str) -> bool {
        self.viewport().iter().any(|line| line.contains(needle))
    }

    /// Number of write calls so far (upstream `terminal.events.length`).
    pub fn write_count(&self) -> usize {
        self.terminal.borrow().write_calls().len()
    }

    /// Whether any write call from `start` on contains `needle`.
    pub fn wrote_since(&self, start: usize, needle: &str) -> bool {
        self.terminal.borrow().write_calls()[start..].iter().any(|write| write.contains(needle))
    }

    pub fn wrote(&self, needle: &str) -> bool {
        self.wrote_since(0, needle)
    }

    pub fn stop(&self) {
        self.tui.stop(TuiStopOptions::default());
    }
}

pub fn wait_ms(milliseconds: u64) {
    pi_tui::ui_loop::run_for(Duration::from_millis(milliseconds)).expect("render");
}

/// `"line 1\nline 2\n..."`, like upstream `Array.from({ length }, ...)`.
pub fn numbered_lines(count: usize) -> String {
    (1..=count).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n")
}

/// Character index of `needle` in `line` (upstream `indexOf` on lines without astral characters).
pub fn column_of(line: &str, needle: &str) -> Option<usize> {
    line.find(needle).map(|index| line[..index].chars().count())
}

/// Character index of the last `needle` in `line` (upstream `lastIndexOf`).
pub fn last_column_of(line: &str, needle: &str) -> Option<usize> {
    line.rfind(needle).map(|index| line[..index].chars().count())
}

/// Wraps a scroll view as a component and returns its shared scroll state.
pub fn scroll_view(child: ComponentRef, options: ScrollViewOptions) -> (ComponentRef, ScrollViewHandle) {
    let view = ScrollView::new(child, options);
    let handle = view.handle();
    (component(view), handle)
}

/// A focusable component that records its keyboard input (upstream inline `editor` objects).
pub struct RecordingEditor {
    pub inputs: Rc<RefCell<Vec<String>>>,
    focused: bool,
}

/// A [`RecordingEditor`] and the list of inputs it receives.
pub fn recording_editor() -> (ComponentRef, Rc<RefCell<Vec<String>>>) {
    let inputs = Rc::new(RefCell::new(Vec::new()));
    let editor: ComponentRef = component(RecordingEditor { inputs: inputs.clone(), focused: false });
    (editor, inputs)
}

impl Component for RecordingEditor {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec!["editor".to_string()]
    }

    fn handle_input(&mut self, data: &str) {
        self.inputs.borrow_mut().push(data.to_string());
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
