//! Test harness for the dialog components: app keybindings, a [`VirtualTerminal`]-backed TUI,
//! key sequences, and ANSI-stripped renders.
#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use pi_tui::env::MapEnv;
use pi_tui::keybindings::KeybindingsConfig;
use pi_tui::testing::VirtualTerminal;
use pi_tui::utils::strip_terminal_sequences;
use pi_tui::{Component, ComponentRef, Tui, TuiOptions, ui_loop};

use crate::keybindings::KeybindingsManager;

/// Key sequences as a terminal sends them (legacy encoding unless noted).
pub mod keys {
    pub const UP: &str = "\x1b[A";
    pub const DOWN: &str = "\x1b[B";
    pub const RIGHT: &str = "\x1b[C";
    pub const LEFT: &str = "\x1b[D";
    pub const ENTER: &str = "\r";
    pub const ESCAPE: &str = "\x1b";
    pub const TAB: &str = "\t";
    pub const SHIFT_TAB: &str = "\x1b[Z";
    pub const BACKSPACE: &str = "\x7f";
    pub const SPACE: &str = " ";
    pub const PAGE_UP: &str = "\x1b[5~";
    pub const PAGE_DOWN: &str = "\x1b[6~";
    pub const HOME: &str = "\x1b[H";
    pub const END: &str = "\x1b[F";
    pub const ALT_UP: &str = "\x1b[1;3A";
    pub const ALT_DOWN: &str = "\x1b[1;3B";
    pub const ALT_LEFT: &str = "\x1b[1;3D";
    pub const ALT_RIGHT: &str = "\x1b[1;3C";
    pub const CTRL_LEFT: &str = "\x1b[1;5D";
    pub const CTRL_RIGHT: &str = "\x1b[1;5C";
    /// Kitty encoding.
    pub const CTRL_BACKSPACE: &str = "\x1b[127;5u";

    /// `ctrl+<letter>` as a control byte.
    pub fn ctrl(letter: char) -> String {
        ((letter.to_ascii_lowercase() as u8 - b'a' + 1) as char).to_string()
    }

    /// `shift+<letter>` in the Kitty encoding.
    pub fn shift(letter: char) -> String {
        format!("\x1b[{};2u", letter.to_ascii_lowercase() as u32)
    }
}

static KEYBINDINGS_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Holds the process-wide keybindings for one test; nested guards on one thread are fine.
pub struct KeybindingsGuard(());

impl Drop for KeybindingsGuard {
    fn drop(&mut self) {
        let depth = DEPTH.with(|depth| {
            depth.set(depth.get() - 1);
            depth.get()
        });
        if depth == 0 {
            HELD.with(|held| held.borrow_mut().take());
        }
    }
}

/// Locks the process-wide keybindings and installs the default app bindings.
pub fn setup() -> KeybindingsGuard {
    setup_with(&[])
}

/// Like [`setup`] with user overrides, such as `[("app.thinking.save", "ctrl+r")]`.
pub fn setup_with(bindings: &[(&str, &str)]) -> KeybindingsGuard {
    if DEPTH.with(Cell::get) == 0 {
        let guard = KEYBINDINGS_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        HELD.with(|held| *held.borrow_mut() = Some(guard));
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    let config: KeybindingsConfig = bindings.iter().map(|(id, key)| (*id, *key)).collect();
    KeybindingsManager::new(config, None).install();
    KeybindingsGuard(())
}

fn nested_guard() -> KeybindingsGuard {
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    KeybindingsGuard(())
}

/// Strips ANSI sequences from rendered lines.
pub fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|line| strip_terminal_sequences(line)).collect()
}

/// Renders a component at `width` and strips ANSI sequences.
pub fn render_plain(component: &mut dyn Component, width: usize) -> Vec<String> {
    plain(&component.render(width))
}

/// [`render_plain`] joined with newlines.
pub fn render_text(component: &mut dyn Component, width: usize) -> String {
    render_plain(component, width).join("\n")
}

/// A component mounted and focused in a main-screen TUI on a [`VirtualTerminal`].
pub struct Harness<C: Component + 'static> {
    _keybindings: KeybindingsGuard,
    pub terminal: Rc<RefCell<VirtualTerminal>>,
    pub tui: Tui,
    pub component: Rc<RefCell<C>>,
}

impl<C: Component + 'static> Harness<C> {
    /// Builds the component with the harness TUI, mounts it, and renders once.
    pub fn new(columns: usize, rows: usize, build: impl FnOnce(&Tui) -> C) -> Self {
        Self::new_shared(columns, rows, |tui| Rc::new(RefCell::new(build(tui))))
    }

    pub fn new_shared(columns: usize, rows: usize, build: impl FnOnce(&Tui) -> Rc<RefCell<C>>) -> Self {
        let keybindings = if DEPTH.with(Cell::get) == 0 { setup() } else { nested_guard() };
        let terminal = VirtualTerminal::shared(columns, rows);
        let options = TuiOptions { env: Some(Rc::new(MapEnv::default())), ..TuiOptions::default() };
        let tui = Tui::new_main_screen(terminal.clone(), options);
        let component = build(&tui);
        let erased: ComponentRef = component.clone();
        tui.add_child(erased.clone());
        tui.set_focus(Some(erased));
        tui.start();
        let harness = Self { _keybindings: keybindings, terminal, tui, component };
        harness.settle();
        harness
    }

    /// Runs pending UI work and renders.
    pub fn settle(&self) {
        ui_loop::run_pending().expect("ui loop");
        self.tui.render_now(false).expect("render");
    }

    /// Runs the UI loop for `duration` (timers and UI tasks), then renders.
    pub fn run_for(&self, duration: Duration) {
        ui_loop::run_for(duration).expect("ui loop");
        self.settle();
    }

    /// Sends input through the terminal and processes it.
    pub fn send(&self, data: &str) {
        self.terminal.borrow().send_input(data);
        self.settle();
    }

    /// Sends several inputs in order.
    pub fn send_all(&self, inputs: &[&str]) {
        for input in inputs {
            self.send(input);
        }
    }

    /// Types text one character at a time.
    pub fn type_text(&self, text: &str) {
        for ch in text.chars() {
            self.send(&ch.to_string());
        }
    }

    /// The visible terminal rows, right-trimmed.
    pub fn viewport(&self) -> Vec<String> {
        self.terminal.borrow().viewport().into_iter().map(|line| line.trim_end().to_string()).collect()
    }

    /// The visible rows joined with newlines.
    pub fn screen(&self) -> String {
        self.viewport().join("\n")
    }

    /// Renders the component directly at `width`, ANSI stripped.
    pub fn render(&self, width: usize) -> Vec<String> {
        render_plain(&mut *self.component.borrow_mut(), width)
    }
}

impl<C: Component + 'static> Drop for Harness<C> {
    fn drop(&mut self) {
        self.tui.stop(Default::default());
    }
}

/// Records callback invocations for assertions.
#[derive(Clone, Default)]
pub struct Calls<T: Clone>(Rc<RefCell<Vec<T>>>);

impl<T: Clone + 'static> Calls<T> {
    pub fn new() -> Self {
        Self(Rc::default())
    }

    pub fn push(&self, value: T) {
        self.0.borrow_mut().push(value);
    }

    pub fn take(&self) -> Vec<T> {
        std::mem::take(&mut self.0.borrow_mut())
    }

    pub fn get(&self) -> Vec<T> {
        self.0.borrow().clone()
    }

    pub fn len(&self) -> usize {
        self.0.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_empty()
    }

    /// A callback that records its argument.
    pub fn recorder(&self) -> impl FnMut(T) + 'static {
        let calls = self.clone();
        move |value| calls.push(value)
    }
}

impl Calls<()> {
    /// A callback without arguments that records each call.
    pub fn counter(&self) -> impl FnMut() + 'static {
        let calls = self.clone();
        move || calls.push(())
    }
}
