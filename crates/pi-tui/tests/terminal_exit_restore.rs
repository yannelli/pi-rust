//! Port of upstream `test/exit-restore.test.ts`. Rust runs no handlers on `std::process::exit`, so
//! the fallback runs when the TUI is dropped or `restore_terminals_on_exit` is called. The backend
//! `write` hook records what a `ProcessTerminal` would write.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard};

use pi_tui::alt_screen::TuiAltScreenOptions;
use pi_tui::env::{MapEnv, global_state_lock};
use pi_tui::keys::set_kitty_protocol_active;
use pi_tui::process_terminal::{ProcessTerminal, ProcessTerminalBackend};
use pi_tui::terminal_image::{TerminalCapabilities, reset_capabilities_cache, set_capabilities};
use pi_tui::{TerminalRef, Tui, TuiOptions, TuiStopOptions, restore_terminals_on_exit};

/// Holds the process-wide state lock and resets the state it pins.
struct Globals {
    _guard: MutexGuard<'static, ()>,
}

impl Globals {
    fn pin() -> Self {
        let guard = global_state_lock();
        set_kitty_protocol_active(false);
        set_capabilities(TerminalCapabilities { images: None, true_color: false, hyperlinks: false });
        Self { _guard: guard }
    }
}

impl Drop for Globals {
    fn drop(&mut self) {
        reset_capabilities_cache();
        set_kitty_protocol_active(false);
    }
}

struct Session {
    tui: Tui,
    writes: Arc<Mutex<Vec<String>>>,
    _globals: Globals,
}

impl Session {
    fn start(fullscreen: bool) -> Self {
        let globals = Globals::pin();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let terminal = process_terminal(&writes);
        let options =
            TuiOptions { env: Some(Rc::new(MapEnv::new([("TERM", "xterm-256color")]))), ..Default::default() };
        let tui = if fullscreen {
            Tui::new_alt_screen(terminal, options, TuiAltScreenOptions { mouse: Some(true), ..Default::default() })
        } else {
            Tui::new_main_screen(terminal, options)
        };
        tui.start();
        Self { tui, writes, _globals: globals }
    }

    fn output(&self) -> String {
        self.writes.lock().unwrap().concat()
    }

    /// Drops the TUI and returns what that wrote.
    fn drop_tui(self) -> String {
        let before = self.output().len();
        let Session { tui, writes, _globals } = self;
        drop(tui);
        writes.lock().unwrap().concat()[before..].to_string()
    }
}

fn process_terminal(writes: &Arc<Mutex<Vec<String>>>) -> TerminalRef {
    let sink = writes.clone();
    Rc::new(RefCell::new(ProcessTerminal::with_backend(ProcessTerminalBackend {
        env: Arc::new(MapEnv::new([("TERM", "xterm-256color")])),
        write: Box::new(move |data: &str| sink.lock().unwrap().push(data.to_string())),
        size: Box::new(|| Some((80, 24))),
        real_io: false,
    })))
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn restores_terminal_modes_when_the_tui_is_dropped_without_stop() {
    let output = Session::start(true).drop_tui();
    for sequence in ["\x1b[?1006l", "\x1b[?1004l", "\x1b[?1003l", "\x1b[?1000l", "\x1b[<u", "\x1b[?2004l"] {
        assert_eq!(count(&output, sequence), 1, "{sequence:?} in {output:?}");
    }
    assert!(output.find("\x1b[<u") < output.find("\x1b[?1049l"), "Kitty pop must precede leaving alt screen");
    assert!(output.ends_with("\x1b[?1049l\x1b[?25h"), "{output:?}");
}

#[test]
fn does_not_restore_twice_after_a_normal_stop() {
    let session = Session::start(true);
    session.tui.stop(TuiStopOptions { preserve_screen: true });
    let stopped = session.output();
    for sequence in ["\x1b[?1006l", "\x1b[?1004l", "\x1b[<u", "\x1b[?2004l", "\x1b[?1049l"] {
        assert_eq!(count(&stopped, sequence), 1, "{sequence:?}");
    }
    assert_eq!(session.drop_tui(), "", "dropping a stopped TUI writes nothing");
}

#[test]
fn restores_once_when_the_exit_hook_runs_before_the_drop() {
    let session = Session::start(true);
    let started = session.output().len();
    restore_terminals_on_exit();
    let restored = session.output()[started..].to_string();
    assert!(restored.ends_with("\x1b[?1049l\x1b[?25h"), "{restored:?}");
    restore_terminals_on_exit();
    assert_eq!(session.output()[started..], restored, "a second call writes nothing");
    assert_eq!(session.drop_tui(), "");
}

#[test]
fn main_screen_exit_restores_the_cursor_and_input_modes_only() {
    let session = Session::start(false);
    session.tui.set_terminal_color_scheme_notifications(true);
    let enabled = session.output().len();
    restore_terminals_on_exit();
    let restored = session.output()[enabled..].to_string();
    assert!(restored.starts_with("\x1b[?2031l\x1b[?25h\x1b[?2004l\x1b[<u"), "{restored:?}");
    assert!(!restored.contains("\x1b[?1049l") && !restored.contains("\x1b[?1000l"), "{restored:?}");
}

#[test]
fn a_tui_that_never_started_restores_nothing() {
    let _globals = Globals::pin();
    let writes = Arc::new(Mutex::new(Vec::new()));
    let tui = Tui::new_main_screen(process_terminal(&writes), TuiOptions::default());
    restore_terminals_on_exit();
    drop(tui);
    assert!(writes.lock().unwrap().is_empty());
}
