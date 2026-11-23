//! Raw mode, window size, and SIGWINCH for the controlling terminal, adapted from crossterm (MIT;
//! see `LICENSE-crossterm`). The first enable saves the mode, and disable writes it back exactly.

use std::io;
use std::sync::{Mutex, MutexGuard, PoisonError};

pub use super::resize::ResizeWatcher;
pub use imp::{disable_raw_mode, enable_raw_mode, restore_mode_after_crash, save_baseline_mode, signal_resize, size};

static PRIOR_MODE: Mutex<Option<imp::Mode>> = Mutex::new(None);

fn prior_mode() -> MutexGuard<'static, Option<imp::Mode>> {
    PRIOR_MODE.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn is_raw_mode_enabled() -> bool {
    prior_mode().is_some()
}

/// Saves the mode from `read` and writes its `raw` form. Does nothing while a mode is saved, and
/// saves nothing when the write fails.
fn enable_saved<M>(
    prior: &mut Option<M>,
    read: impl FnOnce() -> io::Result<M>,
    raw: impl FnOnce(&M) -> M,
    write: impl FnOnce(&M) -> io::Result<()>,
) -> io::Result<()> {
    if prior.is_some() {
        return Ok(());
    }
    let original = read()?;
    write(&raw(&original))?;
    *prior = Some(original);
    Ok(())
}

/// Writes the saved mode back and forgets it. Keeps it when the write fails.
fn disable_saved<M>(prior: &mut Option<M>, write: impl FnOnce(&M) -> io::Result<()>) -> io::Result<()> {
    if let Some(original) = prior.as_ref() {
        write(original)?;
        *prior = None;
    }
    Ok(())
}

/// Console input mode bits that raw mode clears: processed, line, and echo input.
#[cfg(any(windows, test))]
const NOT_RAW_INPUT_MASK: u32 = 0x0001 | 0x0002 | 0x0004;

#[cfg(any(windows, test))]
fn raw_input_mode(mode: u32) -> u32 {
    mode & !NOT_RAW_INPUT_MASK
}

/// Whether `mode` has none of processed, line, and echo input, as crossterm decides raw mode.
#[cfg(any(windows, test))]
fn is_raw_input_mode(mode: u32) -> bool {
    mode & NOT_RAW_INPUT_MASK == 0
}

/// The input mode a crash restore writes, or `None`. A non-raw TUI snapshot wins; a raw live mode
/// goes back to `baseline`, or without one gets processed, line, and echo input back if unsaved.
#[cfg(any(windows, test))]
fn crash_restore_mode(saved: Option<u32>, baseline: Option<u32>, current: impl FnOnce() -> Option<u32>) -> Option<u32> {
    if let Some(saved) = saved {
        return Some(baseline.filter(|_| is_raw_input_mode(saved)).unwrap_or(saved));
    }
    let live = current().filter(|mode| is_raw_input_mode(*mode))?;
    match baseline {
        Some(baseline) => Some(baseline).filter(|baseline| *baseline != live),
        None => Some(live | NOT_RAW_INPUT_MASK),
    }
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsFd, BorrowedFd};

    use rustix::termios::{self, OptionalActions, Termios};

    use super::{disable_saved, enable_saved, prior_mode};
    pub(super) type Mode = Termios;

    /// Runs `f` on stdin when it is a tty, otherwise on a read-write `/dev/tty`.
    fn with_tty<R>(f: impl FnOnce(BorrowedFd<'_>) -> io::Result<R>) -> io::Result<R> {
        let stdin = rustix::stdio::stdin();
        if termios::isatty(stdin) {
            return f(stdin);
        }
        let tty = File::options().read(true).write(true).open("/dev/tty")?;
        f(tty.as_fd())
    }

    /// Saves the terminal mode and switches to raw mode. Does nothing when raw mode is already on.
    pub fn enable_raw_mode() -> io::Result<()> {
        let mut prior = prior_mode();
        if prior.is_some() {
            return Ok(());
        }
        with_tty(|fd| enable_raw_mode_on(fd, &mut prior))
    }

    /// Restores the mode saved by [`enable_raw_mode`], which can itself be a raw mode set by
    /// another program. Does nothing when this process did not enable raw mode.
    pub fn disable_raw_mode() -> io::Result<()> {
        let mut prior = prior_mode();
        if prior.is_none() {
            return Ok(());
        }
        with_tty(|fd| disable_raw_mode_on(fd, &mut prior))
    }

    pub(super) fn enable_raw_mode_on(fd: BorrowedFd<'_>, prior: &mut Option<Termios>) -> io::Result<()> {
        enable_saved(
            prior,
            || Ok(termios::tcgetattr(fd)?),
            |original| {
                let mut raw = original.clone();
                raw.make_raw();
                raw
            },
            |raw| Ok(termios::tcsetattr(fd, OptionalActions::Now, raw)?),
        )
    }

    pub(super) fn disable_raw_mode_on(fd: BorrowedFd<'_>, prior: &mut Option<Termios>) -> io::Result<()> {
        disable_saved(prior, |original| Ok(termios::tcsetattr(fd, OptionalActions::Now, original)?))
    }

    /// Unix crash restores use only the mode [`enable_raw_mode`] saved.
    pub fn save_baseline_mode() {}

    /// Runs `before_write(true)`, then writes back the mode [`enable_raw_mode`] saved, if any.
    pub fn restore_mode_after_crash(before_write: impl FnOnce(bool)) {
        let mut prior = prior_mode();
        if prior.is_none() {
            return;
        }
        before_write(true);
        let _ = with_tty(|fd| disable_raw_mode_on(fd, &mut prior));
    }

    /// `(columns, rows)` of `/dev/tty`, or of stdout when `/dev/tty` cannot be opened. When that
    /// query fails, `tput cols` and `tput lines` give the size.
    pub fn size() -> Option<(u16, u16)> {
        let tty = File::open("/dev/tty");
        let winsize = match &tty {
            Ok(file) => termios::tcgetwinsize(file),
            Err(_) => termios::tcgetwinsize(rustix::stdio::stdout()),
        };
        match winsize {
            Ok(winsize) => Some((winsize.ws_col, winsize.ws_row)),
            Err(_) => tput_value("cols").zip(tput_value("lines")),
        }
    }

    fn tput_value(arg: &str) -> Option<u16> {
        let output = std::process::Command::new("tput").arg(arg).output().ok()?;
        super::parse_tput_value(&output.stdout)
    }

    /// Sends SIGWINCH to this process.
    pub fn signal_resize() -> io::Result<()> {
        use rustix::process::{Signal, getpid, kill_process};
        kill_process(getpid(), Signal::WINCH).map_err(io::Error::from)
    }
}

/// A positive `u16` from `tput` output; anything else is `None`.
#[cfg(any(unix, test))]
fn parse_tput_value(stdout: &[u8]) -> Option<u16> {
    std::str::from_utf8(stdout).ok()?.trim().parse::<u16>().ok().filter(|value| *value > 0)
}

#[cfg(windows)]
mod imp {
    use std::io;

    use crossterm_winapi::{ConsoleMode, Handle, ScreenBuffer};

    use std::sync::OnceLock;

    use super::{crash_restore_mode, disable_saved, enable_saved, prior_mode, raw_input_mode};
    pub(super) type Mode = u32;

    static BASELINE_MODE: OnceLock<u32> = OnceLock::new();

    fn input_mode() -> io::Result<ConsoleMode> {
        Ok(ConsoleMode::from(Handle::current_in_handle()?))
    }

    /// Saves the console input mode and clears processed, line, and echo input. Does nothing when
    /// raw mode is already on.
    pub fn enable_raw_mode() -> io::Result<()> {
        let mut prior = prior_mode();
        if prior.is_some() {
            return Ok(());
        }
        let console = input_mode()?;
        enable_saved(&mut prior, || console.mode(), |mode| raw_input_mode(*mode), |raw| console.set_mode(*raw))
    }

    /// Writes back the exact input mode saved by [`enable_raw_mode`], including bits the host had
    /// already cleared and VT input added after the save.
    pub fn disable_raw_mode() -> io::Result<()> {
        let mut prior = prior_mode();
        if prior.is_none() {
            return Ok(());
        }
        let console = input_mode()?;
        disable_saved(&mut prior, |original| console.set_mode(*original))
    }

    /// Saves the console input mode for [`restore_mode_after_crash`]. The first call wins.
    pub fn save_baseline_mode() {
        if BASELINE_MODE.get().is_none()
            && let Ok(mode) = input_mode().and_then(|console| console.mode())
        {
            let _ = BASELINE_MODE.set(mode);
        }
    }

    /// Writes the mode from [`crash_restore_mode`] after `before_write(tui_saved)`; nothing if `None`.
    pub fn restore_mode_after_crash(before_write: impl FnOnce(bool)) {
        let mut prior = prior_mode();
        let Ok(console) = input_mode() else { return };
        let Some(mode) = crash_restore_mode(*prior, BASELINE_MODE.get().copied(), || console.mode().ok()) else {
            return;
        };
        before_write(prior.is_some());
        if console.set_mode(mode).is_ok() {
            *prior = None;
        }
    }

    /// `(columns, rows)` of the visible console window.
    pub fn size() -> Option<(u16, u16)> {
        let window = ScreenBuffer::current().ok()?.info().ok()?.terminal_size();
        Some(((window.width + 1) as u16, (window.height + 1) as u16))
    }

    /// Windows has no SIGWINCH; console input records report resizes.
    pub fn signal_resize() -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "tty_tests.rs"]
mod tests;
