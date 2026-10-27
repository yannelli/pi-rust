//! The [`Terminal`] trait and helpers shared by terminal implementations.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;

use crate::env::{EnvLookup, ProcessEnv};
use crate::program_status::ProgramStatus;
use crate::ui_loop::TerminalSink;
use crate::utils::replace_control_characters;

pub use crate::process_terminal::ProcessTerminal;

/// Shared handle to a terminal. A TUI that takes over from another reuses the same handle.
pub type TerminalRef = Rc<RefCell<dyn Terminal>>;

/// Minimal terminal interface for the TUI. Tests implement it with a virtual terminal.
pub trait Terminal {
    /// Starts the terminal. Input sequences and resize notifications go to `sink`.
    fn start(&mut self, sink: TerminalSink);

    /// Stops the terminal and restores its state.
    fn stop(&mut self);

    /// Restores terminal modes synchronously when the TUI goes away without [`Terminal::stop`].
    /// Writes `before`, then this terminal's own mode resets, then `after`.
    fn restore_on_exit(&mut self, _before: &str, _after: &str) {}

    /// Drains stdin before exit so Kitty key release events do not leak to the parent shell.
    /// Waits at most `max`, and returns early when no input arrives within `idle`.
    fn drain_input(&mut self, _max: Duration, _idle: Duration) {}

    /// Writes output to the terminal.
    fn write(&mut self, data: &str);

    /// Terminal width in columns.
    fn columns(&self) -> usize;

    /// Terminal height in rows.
    fn rows(&self) -> usize;

    /// Whether the Kitty keyboard protocol is active.
    fn kitty_protocol_active(&self) -> bool;

    /// Moves the cursor up (negative) or down (positive) by `lines`.
    fn move_by(&mut self, lines: i32) {
        if lines > 0 {
            self.write(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            self.write(&format!("\x1b[{}A", -lines));
        }
    }

    fn hide_cursor(&mut self) {
        self.write("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        self.write("\x1b[?25h");
    }

    /// Clears the current line.
    fn clear_line(&mut self) {
        self.write("\x1b[K");
    }

    /// Clears from the cursor to the end of the screen.
    fn clear_from_cursor(&mut self) {
        self.write("\x1b[J");
    }

    /// Clears the screen and moves the cursor home.
    fn clear_screen(&mut self) {
        self.write("\x1b[2J\x1b[H");
    }

    /// Sets the window title (OSC 0). A BEL or ESC in the title would end the sequence early and send the
    /// rest to the terminal as escape sequences (session names come from session files).
    fn set_title(&mut self, title: &str) {
        self.write(&format!("\x1b]0;{}\x07", replace_control_characters(title)));
    }

    /// Shows or clears the indeterminate progress indicator (OSC 9;4).
    fn set_progress(&mut self, _active: bool) {}

    /// Reports what the program is doing (OSC 7501), only to terminals that support it.
    fn set_program_status(&mut self, _status: &ProgramStatus) {}
}

/// A keyboard protocol negotiation reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    /// `CSI ? flags u`: the Kitty keyboard flags in effect.
    KittyFlags(u32),
    /// A primary device attributes (DA1) reply.
    DeviceAttributes,
}

static KITTY_FLAGS_REPLY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\x1b\[\?(\d+)u$").expect("valid regex"));
static DEVICE_ATTRIBUTES_REPLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\x1b\[\?[\d;]*c$").expect("valid regex"));
static NEGOTIATION_PREFIX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\x1b\[\?[\d;]*$").expect("valid regex"));

/// Parses a Kitty flags reply or a DA1 reply.
pub fn parse_keyboard_protocol_negotiation_sequence(sequence: &str) -> Option<KeyboardProtocolNegotiationSequence> {
    if let Some(captures) = KITTY_FLAGS_REPLY.captures(sequence) {
        let flags = captures[1].parse::<u64>().unwrap_or(u64::MAX).min(u32::MAX as u64) as u32;
        return Some(KeyboardProtocolNegotiationSequence::KittyFlags(flags));
    }
    if DEVICE_ATTRIBUTES_REPLY.is_match(sequence) {
        return Some(KeyboardProtocolNegotiationSequence::DeviceAttributes);
    }
    None
}

pub(crate) fn is_keyboard_protocol_negotiation_sequence_prefix(sequence: &str) -> bool {
    sequence == "\x1b[" || NEGOTIATION_PREFIX.is_match(sequence)
}

/// Whether this process runs in macOS Terminal.app.
pub fn is_apple_terminal_session() -> bool {
    is_apple_terminal_session_with(&ProcessEnv)
}

/// [`is_apple_terminal_session`] with an explicit environment.
pub fn is_apple_terminal_session_with(env: &dyn EnvLookup) -> bool {
    cfg!(target_os = "macos") && env.var("TERM_PROGRAM").as_deref() == Some("Apple_Terminal")
}

/// Refreshes terminal dimensions on POSIX by sending SIGWINCH to this process. Best effort:
/// environments that forbid signal delivery are ignored.
pub fn refresh_terminal_dimensions() {
    refresh_terminal_dimensions_with(cfg!(windows), || {
        #[cfg(unix)]
        {
            signal_hook::low_level::raise(signal_hook::consts::SIGWINCH)
        }
        #[cfg(not(unix))]
        {
            Ok(())
        }
    });
}

/// [`refresh_terminal_dimensions`] with the platform check and the self-signal injected, so tests
/// can simulate Windows and failed signal delivery. Errors from `signal_self` are ignored.
#[doc(hidden)]
pub fn refresh_terminal_dimensions_with(is_windows: bool, signal_self: impl FnOnce() -> std::io::Result<()>) {
    if is_windows {
        return;
    }
    let _ = signal_self();
}

const NATIVE_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";

/// Rewrites Return to CSI-u Shift+Enter when native Shift detection is enabled and Shift is held.
pub fn normalize_native_shift_enter_input(data: &str, detect_native_shift_enter: bool, shift_pressed: bool) -> String {
    if detect_native_shift_enter && data == "\r" && shift_pressed {
        NATIVE_SHIFT_ENTER_SEQUENCE.to_string()
    } else {
        data.to_string()
    }
}

/// Apple Terminal variant of [`normalize_native_shift_enter_input`].
pub fn normalize_apple_terminal_input(data: &str, is_apple_terminal: bool, shift_pressed: bool) -> String {
    normalize_native_shift_enter_input(data, is_apple_terminal, shift_pressed)
}

const DEFAULT_ESCAPE_TIMEOUT_MS: u64 = 10;
const DEFAULT_SSH_ESCAPE_TIMEOUT_MS: u64 = 100;

/// How long to wait for the rest of an escape sequence before a lone ESC counts as Escape.
/// `PI_TUI_ESC_TIMEOUT` overrides it; SSH sessions default to a longer window.
pub fn resolve_escape_timeout_ms(env: &dyn EnvLookup) -> u64 {
    if let Some(configured) = env.var("PI_TUI_ESC_TIMEOUT").and_then(|v| v.trim().parse::<f64>().ok())
        && configured.is_finite()
        && configured > 0.0
    {
        return configured.ceil() as u64;
    }
    if env.non_empty("SSH_CONNECTION").is_some() || env.non_empty("SSH_TTY").is_some() {
        return DEFAULT_SSH_ESCAPE_TIMEOUT_MS;
    }
    DEFAULT_ESCAPE_TIMEOUT_MS
}
