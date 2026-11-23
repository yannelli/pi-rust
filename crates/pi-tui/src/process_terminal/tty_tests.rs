use std::cell::{Cell, RefCell};
use std::io;

use super::{NOT_RAW_INPUT_MASK, crash_restore_mode, disable_saved, enable_saved, parse_tput_value, raw_input_mode};

const PROCESSED: u32 = 0x0001;
const LINE: u32 = 0x0002;
const ECHO: u32 = 0x0004;
const WINDOW_INPUT: u32 = 0x0008;
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const EXTENDED_FLAGS: u32 = 0x0080;
/// A fresh conhost input mode: insert, quick edit, and extended flags on top of cooked input.
const COOKED: u32 = PROCESSED | LINE | ECHO | 0x0020 | 0x0040 | EXTENDED_FLAGS;
/// The mode pi-js writes for `setRawMode(true)`, as libuv does.
const EXTENSION_RAW: u32 = WINDOW_INPUT;

/// A console input mode that records every write.
struct FakeConsole {
    mode: Cell<u32>,
    writes: RefCell<Vec<u32>>,
    fail_writes: bool,
}

impl FakeConsole {
    fn new(mode: u32) -> Self {
        Self { mode: Cell::new(mode), writes: RefCell::new(Vec::new()), fail_writes: false }
    }

    fn enable(&self, prior: &mut Option<u32>) -> io::Result<()> {
        enable_saved(prior, || Ok(self.mode.get()), |mode| raw_input_mode(*mode), |raw| self.write(*raw))
    }

    fn disable(&self, prior: &mut Option<u32>) -> io::Result<()> {
        disable_saved(prior, |original| self.write(*original))
    }

    fn write(&self, mode: u32) -> io::Result<()> {
        if self.fail_writes {
            return Err(io::Error::other("SetConsoleMode failed"));
        }
        self.writes.borrow_mut().push(mode);
        self.mode.set(mode);
        Ok(())
    }
}

#[test]
fn raw_input_mode_clears_only_processed_line_and_echo() {
    assert_eq!(NOT_RAW_INPUT_MASK, PROCESSED | LINE | ECHO);
    let cooked = PROCESSED | LINE | ECHO | WINDOW_INPUT | VIRTUAL_TERMINAL_INPUT;
    assert_eq!(raw_input_mode(cooked), WINDOW_INPUT | VIRTUAL_TERMINAL_INPUT);
    assert_eq!(raw_input_mode(0), 0);
}

#[test]
fn restore_keeps_raw_bits_the_host_had_already_cleared() {
    let host = PROCESSED | LINE;
    let console = FakeConsole::new(host);
    let mut prior = None;
    console.enable(&mut prior).unwrap();
    assert_eq!(console.mode.get(), 0);
    console.mode.set(console.mode.get() | VIRTUAL_TERMINAL_INPUT | WINDOW_INPUT);
    console.disable(&mut prior).unwrap();
    assert_eq!(console.mode.get(), host, "echo stays off and VT input is removed");
    assert_eq!(prior, None);
}

#[test]
fn enable_twice_keeps_the_first_saved_mode_and_disable_without_enable_writes_nothing() {
    let console = FakeConsole::new(PROCESSED | LINE | ECHO);
    let mut prior = None;
    console.disable(&mut prior).unwrap();
    assert!(console.writes.borrow().is_empty());
    console.enable(&mut prior).unwrap();
    console.enable(&mut prior).unwrap();
    assert_eq!(*console.writes.borrow(), [0]);
    assert_eq!(prior, Some(PROCESSED | LINE | ECHO));
}

#[test]
fn failed_writes_leave_the_saved_state_unchanged() {
    let mut console = FakeConsole::new(PROCESSED | LINE | ECHO);
    console.fail_writes = true;
    let mut prior = None;
    assert!(console.enable(&mut prior).is_err());
    assert_eq!(prior, None, "a failed enable saves nothing");
    prior = Some(LINE);
    assert!(console.disable(&mut prior).is_err());
    assert_eq!(prior, Some(LINE), "a failed restore keeps the mode for a retry");
}

fn crash_mode(saved: Option<u32>, baseline: Option<u32>, current: u32) -> Option<u32> {
    crash_restore_mode(saved, baseline, || Some(current))
}

#[test]
fn crash_restore_undoes_extension_raw_mode_outside_the_tui() {
    assert_eq!(crash_mode(None, Some(COOKED), EXTENSION_RAW), Some(COOKED));
}

#[test]
fn crash_restore_leaves_a_console_that_is_not_raw() {
    assert_eq!(crash_mode(None, Some(COOKED), COOKED), None);
    let quick_edit_off = COOKED & !0x0040;
    assert_eq!(crash_mode(None, Some(COOKED), quick_edit_off), None, "unrelated host changes stay");
    assert_eq!(crash_mode(None, Some(COOKED), PROCESSED), None, "a partly cooked console stays");
    assert_eq!(crash_mode(None, None, COOKED), None);
}

#[test]
fn crash_restore_keeps_a_host_that_started_raw() {
    let host_raw = raw_input_mode(COOKED);
    assert_eq!(crash_mode(None, Some(host_raw), host_raw), None);
    assert_eq!(crash_mode(None, Some(host_raw), EXTENSION_RAW), Some(host_raw), "host bits come back");
    assert_eq!(crash_mode(Some(host_raw), None, 0), Some(host_raw), "no cooked bits without a baseline");
    assert_eq!(crash_mode(Some(host_raw), Some(host_raw), 0), Some(host_raw));
}

#[test]
fn crash_restore_reaches_the_owner_mode_in_either_tui_and_extension_order() {
    let tui_first = crash_mode(Some(COOKED & !0x0040), Some(COOKED), EXTENSION_RAW);
    assert_eq!(tui_first, Some(COOKED & !0x0040), "a cooked TUI snapshot is newer than the baseline");
    let extension_first = crash_mode(Some(EXTENSION_RAW), Some(COOKED), raw_input_mode(EXTENSION_RAW));
    assert_eq!(extension_first, Some(COOKED), "the TUI snapshot is the extension's raw mode");
}

#[test]
fn crash_restore_without_a_baseline_sets_the_cooked_bits_on_a_raw_console() {
    assert_eq!(crash_mode(None, None, EXTENSION_RAW), Some(EXTENSION_RAW | NOT_RAW_INPUT_MASK));
    assert_eq!(crash_mode(Some(COOKED), None, 0), Some(COOKED), "the TUI snapshot is written back");
}

#[test]
fn crash_restore_skips_the_read_with_a_saved_mode_and_stops_when_the_read_fails() {
    let saved = crash_restore_mode(Some(COOKED), Some(COOKED), || panic!("the live mode is the saved mode"));
    assert_eq!(saved, Some(COOKED));
    assert_eq!(crash_restore_mode(None, Some(COOKED), || None), None);
}

#[test]
fn parses_tput_output() {
    assert_eq!(parse_tput_value(b"120\n"), Some(120));
    assert_eq!(parse_tput_value(b" 80 "), Some(80));
    for malformed in [&b""[..], b"0\n", b"-5", b"12abc", b"70000", b"\xff"] {
        assert_eq!(parse_tput_value(malformed), None, "{malformed:?}");
    }
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::os::fd::{AsFd, OwnedFd};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::time::Duration;

    use rustix::fs::{Mode, OFlags};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    use rustix::termios::{self, LocalModes, OptionalActions, Termios};

    use super::super::ResizeWatcher;
    use super::super::imp::{disable_raw_mode_on, enable_raw_mode_on, signal_resize};

    const WAIT: Duration = Duration::from_secs(5);

    /// A pty pair; the slave stands in for the controlling terminal.
    fn open_pty() -> (OwnedFd, File) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let name = ptsname(&master, Vec::new()).expect("ptsname");
        let slave =
            rustix::fs::open(name.as_c_str(), OFlags::RDWR | OFlags::NOCTTY, Mode::empty()).expect("open slave");
        (master, File::from(slave))
    }

    /// Compares the mode flags. PENDIN is excluded: the macOS kernel sets it when canonical mode returns.
    fn same_mode(a: &Termios, b: &Termios) -> bool {
        a.input_modes == b.input_modes
            && a.output_modes == b.output_modes
            && a.control_modes == b.control_modes
            && a.local_modes - LocalModes::PENDIN == b.local_modes - LocalModes::PENDIN
    }

    #[test]
    fn raw_mode_round_trip_restores_the_saved_mode() {
        let (_master, slave) = open_pty();
        let original = termios::tcgetattr(&slave).unwrap();
        assert!(original.local_modes.contains(LocalModes::ICANON));
        let mut prior = None;
        enable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
        let raw = termios::tcgetattr(&slave).unwrap();
        assert!(!raw.local_modes.intersects(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG));
        enable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
        assert!(same_mode(prior.as_ref().unwrap(), &original), "a second enable keeps the first saved mode");
        disable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
        assert!(prior.is_none());
        let restored = termios::tcgetattr(&slave).unwrap();
        assert!(same_mode(&restored, &original), "{restored:?}\n{original:?}");
        disable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
    }

    #[test]
    fn raw_mode_set_by_another_program_survives_restore() {
        let (_master, slave) = open_pty();
        let mut external = termios::tcgetattr(&slave).unwrap();
        external.make_raw();
        termios::tcsetattr(&slave, OptionalActions::Now, &external).unwrap();
        let mut prior = None;
        enable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
        disable_raw_mode_on(slave.as_fd(), &mut prior).unwrap();
        let after = termios::tcgetattr(&slave).unwrap();
        assert!(same_mode(&after, &external));
        assert!(!after.local_modes.contains(LocalModes::ICANON));
    }

    fn watch() -> (ResizeWatcher, Receiver<()>) {
        let (send, received) = mpsc::channel();
        let watcher = ResizeWatcher::spawn(move || send.send(()).unwrap()).expect("watcher starts");
        (watcher, received)
    }

    #[test]
    fn watcher_sees_a_signal_sent_right_after_spawn_and_stop_joins() {
        let (watcher, received) = watch();
        signal_resize().unwrap();
        received.recv_timeout(WAIT).expect("resize callback");
        watcher.stop();
        for _ in received.try_iter() {}
        assert_eq!(received.try_recv(), Err(mpsc::TryRecvError::Disconnected));
    }

    #[test]
    fn dropping_the_watcher_ends_its_thread() {
        let (watcher, received) = watch();
        drop(watcher);
        loop {
            match received.recv_timeout(WAIT) {
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => panic!("thread kept the callback after drop"),
                Ok(()) => continue,
            }
        }
    }

    #[tokio::test]
    async fn watcher_starts_inside_a_tokio_runtime() {
        let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
        let watcher = ResizeWatcher::spawn(move || send.send(()).unwrap()).expect("watcher starts");
        signal_resize().unwrap();
        assert_eq!(tokio::time::timeout(WAIT, received.recv()).await.unwrap(), Some(()));
        watcher.stop();
    }

    #[test]
    fn two_watchers_both_see_one_signal() {
        let (first, first_signal) = watch();
        let (second, second_signal) = watch();
        signal_resize().unwrap();
        first_signal.recv_timeout(WAIT).expect("first watcher");
        second_signal.recv_timeout(WAIT).expect("second watcher");
        first.stop();
        second.stop();
    }
}
