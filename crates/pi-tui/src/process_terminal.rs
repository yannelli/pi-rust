//! [`ProcessTerminal`]: the real terminal on stdin and stdout.

mod input;
mod reader;

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use input::{Core, OutputFn, TERMINAL_PROGRESS_ACTIVE_SEQUENCE, TERMINAL_PROGRESS_CLEAR_SEQUENCE};
use reader::{ReaderThread, ResizeWatcher, spawn_reader};
pub use reader::{Utf8Decoder, decode_utf16_units};

use crate::env::{EnvLookup, ProcessEnv};
use crate::program_status::{ProgramState, ProgramStatus, format_program_status};
use crate::terminal::{
    Terminal, is_apple_terminal_session_with, refresh_terminal_dimensions, resolve_escape_timeout_ms,
};
use crate::ui_loop::{self, TerminalSink, TimerId};

const TERMINAL_PROGRESS_KEEPALIVE: Duration = Duration::from_secs(1);

/// Environment shared with background threads.
pub type SharedEnv = Arc<dyn EnvLookup + Send + Sync>;
/// Terminal size query returning `(columns, rows)`.
pub type SizeQuery = Box<dyn Fn() -> Option<(usize, usize)>>;

/// I/O used by a [`ProcessTerminal`]. [`ProcessTerminal::new`] uses the real stdio; tests pass
/// their own writer and size query and drive input with [`ProcessTerminal::feed_input`].
pub struct ProcessTerminalBackend {
    pub env: SharedEnv,
    pub write: Box<dyn FnMut(&str) + Send>,
    pub size: SizeQuery,
    /// Whether `start` switches stdin to raw mode and starts the input and resize threads.
    pub real_io: bool,
}

/// Real terminal on the process stdin and stdout. Input arrives as raw bytes on a background
/// thread, is split into sequences like upstream, and is delivered to the UI loop.
pub struct ProcessTerminal {
    core: Arc<Mutex<Core>>,
    env: SharedEnv,
    size: SizeQuery,
    real_io: bool,
    was_raw: bool,
    raw_enabled: bool,
    reader: Option<ReaderThread>,
    resize: Option<ResizeWatcher>,
    progress_timer: Option<TimerId>,
    write_log_path: Option<PathBuf>,
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

fn stdout_write(data: &str) {
    let mut out = std::io::stdout().lock();
    if let Err(error) = out.write_all(data.as_bytes()).and_then(|()| out.flush()) {
        report_terminal_error(&error);
    }
}

static DEAD_TERMINAL_HANDLER: std::sync::Mutex<Option<fn()>> = std::sync::Mutex::new(None);

/// Run `handler` when terminal I/O fails because the terminal is gone (see
/// [`is_dead_terminal_error`]); `None` removes it. The handler is expected to exit.
pub fn set_dead_terminal_handler(handler: Option<fn()>) {
    *DEAD_TERMINAL_HANDLER.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = handler;
}

/// Whether an I/O error means the terminal is gone: EIO from an orphaned process group, EPIPE,
/// ENOTCONN, or ENOTTY from a revoked tty.
pub fn is_dead_terminal_error(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error()
        && [rustix::io::Errno::IO, rustix::io::Errno::NOTTY].iter().any(|errno| errno.raw_os_error() == code)
    {
        return true;
    }
    matches!(error.kind(), std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::NotConnected)
}

/// Call the dead terminal handler when `error` is a dead terminal error.
pub(crate) fn report_terminal_error(error: &std::io::Error) {
    if !is_dead_terminal_error(error) {
        return;
    }
    let handler = *DEAD_TERMINAL_HANDLER.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(handler) = handler {
        handler();
    }
}

fn real_size() -> Option<(usize, usize)> {
    crossterm::terminal::size().ok().filter(|(c, r)| *c > 0 && *r > 0).map(|(c, r)| (c as usize, r as usize))
}

impl ProcessTerminal {
    /// Terminal on the real stdin and stdout.
    pub fn new() -> Self {
        Self::with_backend(ProcessTerminalBackend {
            env: Arc::new(ProcessEnv),
            write: Box::new(stdout_write),
            size: Box::new(real_size),
            real_io: true,
        })
    }

    /// Terminal with injected I/O.
    pub fn with_backend(backend: ProcessTerminalBackend) -> Self {
        let env = backend.env;
        let escape_timeout = Duration::from_millis(resolve_escape_timeout_ms(env.as_ref()));
        let detect_shift_enter = is_apple_terminal_session_with(env.as_ref()) || cfg!(windows);
        let output: OutputFn = backend.write;
        let write_log_path = resolve_write_log_path(env.as_ref());
        Self {
            core: Arc::new(Mutex::new(Core::new(output, escape_timeout, detect_shift_enter))),
            env,
            size: backend.size,
            real_io: backend.real_io,
            was_raw: false,
            raw_enabled: false,
            reader: None,
            resize: None,
            progress_timer: None,
            write_log_path,
        }
    }

    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether the xterm modifyOtherKeys fallback is enabled.
    pub fn modify_other_keys_active(&self) -> bool {
        self.core().modify_other_keys_active
    }

    /// Installs the input handler and sends the keyboard protocol queries, without touching stdin.
    /// `start` calls this; tests use it with [`ProcessTerminal::feed_input`].
    pub fn begin_input(&mut self, handler: impl FnMut(String) + Send + 'static) {
        let override_value = self.env.var("PI_PROGRAM_STATUS");
        let mut core = self.core();
        core.handler = Some(Box::new(handler));
        core.query_and_enable_kitty_protocol(override_value.as_deref());
    }

    /// Feeds raw stdin text, as the input thread does.
    pub fn feed_input(&self, data: &str, now: Instant) {
        self.core().feed(data, now);
    }

    /// Fires due input timeouts (lone ESC, incomplete sequences, split negotiation replies).
    pub fn poll_input_timers(&self, now: Instant) {
        self.core().poll_timers(now);
    }

    fn enable_raw_mode(&mut self) {
        self.was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
        self.raw_enabled = crossterm::terminal::enable_raw_mode().is_ok();
        #[cfg(windows)]
        enable_windows_console_modes();
    }

    fn restore_raw_mode(&mut self) {
        if self.raw_enabled && !self.was_raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
        self.raw_enabled = false;
    }

    fn clear_progress_interval(&mut self) -> bool {
        match self.progress_timer.take() {
            Some(timer) => {
                ui_loop::clear_timer(timer);
                true
            }
            None => false,
        }
    }
}

/// Adds VT input, window input, and VT output processing to the console modes.
#[cfg(windows)]
fn enable_windows_console_modes() {
    use crossterm_winapi::{ConsoleMode, Handle};
    const ENABLE_WINDOW_INPUT: u32 = 0x0008;
    const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    if let Ok(mode) = Handle::current_in_handle().map(ConsoleMode::from)
        && let Ok(current) = mode.mode()
    {
        let _ = mode.set_mode(current | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_WINDOW_INPUT);
    }
    if let Ok(mode) = Handle::current_out_handle().map(ConsoleMode::from)
        && let Ok(current) = mode.mode()
    {
        let _ = mode.set_mode(current | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

fn resolve_write_log_path(env: &dyn EnvLookup) -> Option<PathBuf> {
    let configured = env.non_empty("PI_TUI_WRITE_LOG")?;
    let path = PathBuf::from(&configured);
    if path.is_dir() {
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let name = format!("tui-{}-{}.log", format_timestamp(secs), std::process::id());
        return Some(path.join(name));
    }
    Some(path)
}

/// Formats Unix seconds as `YYYY-MM-DD_HH-MM-SS` (UTC).
fn format_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}_{:02}-{:02}-{:02}", rem / 3600, (rem / 60) % 60, rem % 60)
}

impl Terminal for ProcessTerminal {
    fn start(&mut self, sink: TerminalSink) {
        if self.real_io {
            self.enable_raw_mode();
        }
        self.core().write("\x1b[?2004h");
        if self.real_io {
            self.resize = ResizeWatcher::spawn(sink.clone());
            refresh_terminal_dimensions();
        }
        let input_sink = sink.clone();
        self.begin_input(move |data| input_sink.input(data));
        if self.real_io {
            self.reader = Some(spawn_reader(self.core.clone(), sink));
        }
    }

    fn stop(&mut self) {
        if self.clear_progress_interval() {
            self.core().write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
        {
            let mut core = self.core();
            if core.program_status_supported && core.program_status.is_some() {
                let clear = ProgramStatus { state: ProgramState::Clear, app: None, kind: None, message: None };
                let encoded = format_program_status(&clear);
                core.write(&encoded);
            }
            core.program_status_supported = false;
            core.program_status_query_pending = false;
            core.write("\x1b[?2004l");
            core.clear_negotiation_buffer();
            core.disable_kitty_protocol();
            core.disable_modify_other_keys();
            core.stdin_buffer = None;
            core.handler = None;
        }
        if let Some(reader) = self.reader.take() {
            reader.stop();
        }
        if let Some(resize) = self.resize.take() {
            resize.stop();
        }
        self.restore_raw_mode();
    }

    fn restore_on_exit(&mut self, before: &str, after: &str) {
        let mut sequence = before.to_string();
        if self.progress_timer.is_some() {
            sequence.push_str(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
        sequence.push_str("\x1b[?2004l");
        {
            let core = self.core();
            if core.keyboard_protocol_pushed || core.kitty_active {
                sequence.push_str("\x1b[<u");
            }
            if core.modify_other_keys_active {
                sequence.push_str("\x1b[>4;0m");
            }
        }
        sequence.push_str(after);
        self.core().write(&sequence);
        self.restore_raw_mode();
    }

    fn drain_input(&mut self, max: Duration, idle: Duration) {
        {
            let mut core = self.core();
            core.clear_negotiation_buffer();
            core.disable_kitty_protocol();
            core.disable_modify_other_keys();
            core.draining = true;
            core.last_data_at = Instant::now();
        }
        let end = Instant::now() + max;
        loop {
            let now = Instant::now();
            if now >= end {
                break;
            }
            let last = self.core().last_data_at;
            if now.saturating_duration_since(last) >= idle {
                break;
            }
            std::thread::sleep(idle.min(end - now));
        }
        self.core().draining = false;
    }

    fn write(&mut self, data: &str) {
        self.core().write(data);
        if let Some(path) = &self.write_log_path
            && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
        {
            let _ = file.write_all(data.as_bytes());
        }
    }

    fn columns(&self) -> usize {
        (self.size)()
            .map(|(c, _)| c)
            .or_else(|| self.env.var("COLUMNS").and_then(|v| v.trim().parse().ok()).filter(|v: &usize| *v > 0))
            .unwrap_or(80)
    }

    fn rows(&self) -> usize {
        (self.size)()
            .map(|(_, r)| r)
            .or_else(|| self.env.var("LINES").and_then(|v| v.trim().parse().ok()).filter(|v: &usize| *v > 0))
            .unwrap_or(24)
    }

    fn kitty_protocol_active(&self) -> bool {
        self.core().kitty_active
    }

    fn set_progress(&mut self, active: bool) {
        if active {
            self.core().write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            if self.progress_timer.is_none() {
                let core = self.core.clone();
                self.progress_timer = Some(ui_loop::set_interval(TERMINAL_PROGRESS_KEEPALIVE, move || {
                    if let Ok(mut core) = core.lock() {
                        core.write(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
                    }
                }));
            }
        } else {
            self.clear_progress_interval();
            self.core().write(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }

    fn set_program_status(&mut self, status: &ProgramStatus) {
        self.core().set_program_status(status);
    }
}

/// Put the terminal back after a crash, when a TUI left it in raw mode: leave the alternate
/// screen, turn off mouse, paste, and keyboard protocols, show the cursor, and leave raw mode.
/// Does nothing when raw mode is off, so it is safe to call from a panic hook.
pub fn restore_terminal_after_crash() {
    if !crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        return;
    }
    let sequence = "\x1b[?2026l\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[?2031l\
                    \x1b[<u\x1b[>4;0m\x1b[?7h\x1b[?1049l\x1b[0m\x1b[?25h\r\n";
    let mut stdout = std::io::stdout();
    let _ = std::io::Write::write_all(&mut stdout, sequence.as_bytes());
    let _ = std::io::Write::flush(&mut stdout);
    let _ = crossterm::terminal::disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::format_timestamp;

    #[test]
    fn formats_utc_timestamps() {
        assert_eq!(format_timestamp(0), "1970-01-01_00-00-00");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14_22-13-20");
    }
}
