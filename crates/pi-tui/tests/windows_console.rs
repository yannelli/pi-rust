//! Raw mode and crash restore on a real Win32 console: each case re-runs this binary in its own
//! classic conhost (`CREATE_NEW_CONSOLE`) or ConPTY (`scripts/windows-conpty-run.ps1`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use crossterm_winapi::{ConsoleMode, Handle};
use pi_tui::env::MapEnv;
use pi_tui::process_terminal::{
    ProcessTerminal, ProcessTerminalBackend, restore_terminal_after_crash, save_terminal_mode_for_crash,
};
use pi_tui::{Tui, TuiOptions, TuiStopOptions};

const SCENARIO: &str = "PI_TUI_CONSOLE_SCENARIO";
const REPORT: &str = "PI_TUI_CONSOLE_REPORT";
/// Set in a ConPTY child: files the child drops here go to the ConPTY input pipe.
pub(crate) const CONPTY_INPUT_DIR: &str = "PI_CONPTY_INPUT_DIR";
const CHILD_TEST: &str = "windows_console::console_child";
pub(crate) const COOKED: u32 = 0x0001 | 0x0002 | 0x0004;
const WINDOW_INPUT: u32 = 0x0008;
const QUICK_EDIT: u32 = 0x0040;
const EXTENDED_FLAGS: u32 = 0x0080;
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
/// What pi-js writes for `process.stdin.setRawMode(true)`, as libuv does.
const EXTENSION_RAW: u32 = WINDOW_INPUT;
const CHILD_DEADLINE: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, Debug)]
pub(crate) enum Backend {
    Classic,
    ConPty,
}

pub(crate) struct Child {
    report: HashMap<String, u32>,
    exit: Option<i32>,
    pub(crate) log: String,
}

impl Child {
    pub(crate) fn mode(&self, key: &str) -> u32 {
        *self.report.get(key).unwrap_or_else(|| panic!("no {key} in {:x?}\n{}", self.report, self.log))
    }

    pub(crate) fn assert_exit(&self, crashed: bool) {
        let expected = if crashed { 101 } else { 0 };
        assert_eq!(self.exit, Some(expected), "{}", self.log);
    }
}

pub(crate) fn run(scenario: &str, backend: Backend) -> Child {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.txt");
    let exe = std::env::current_exe().unwrap();
    let test_args = ["--exact", CHILD_TEST, "--ignored", "--nocapture", "--test-threads=1"];
    let mut command = match backend {
        Backend::Classic => {
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            let mut command = Command::new(&exe);
            command.args(test_args).creation_flags(CREATE_NEW_CONSOLE);
            command
        }
        Backend::ConPty => {
            let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/windows-conpty-run.ps1");
            let command_line = format!("\"{}\" {}", exe.display(), test_args.join(" "));
            let mut command = Command::new("powershell.exe");
            command.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"]);
            let input = dir.path().join("input");
            std::fs::create_dir(&input).unwrap();
            command.arg(script).env("PI_CONPTY_COMMAND_LINE", command_line).env(CONPTY_INPUT_DIR, input);
            command
        }
    };
    command.env(SCENARIO, scenario).env(REPORT, &report);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = command.spawn().expect("spawn console child");
    let pid = child.id();
    let (send, done) = mpsc::channel();
    std::thread::spawn(move || send.send(child.wait_with_output()));
    let output = match done.recv_timeout(CHILD_DEADLINE) {
        Ok(output) => output.expect("wait for console child"),
        Err(_) => {
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).status();
            panic!("{scenario} on {backend:?} ran longer than {CHILD_DEADLINE:?}");
        }
    };
    let log = format!(
        "{scenario} on {backend:?}, exit {:?}\nnotes:\n{}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        std::fs::read_to_string(report.with_extension("notes")).unwrap_or_default(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = std::fs::read_to_string(&report).unwrap_or_default();
    let report = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), u32::from_str_radix(value, 16).expect("hex mode")))
        .collect();
    Child { report, exit: output.status.code(), log }
}

fn console() -> ConsoleMode {
    ConsoleMode::from(Handle::current_in_handle().expect("CONIN$ opens in the console child"))
}

pub(crate) fn read_mode() -> u32 {
    console().mode().expect("GetConsoleMode")
}

/// Writes `mode` and returns what the console kept.
fn seed(mode: u32) -> u32 {
    console().set_mode(mode).expect("SetConsoleMode");
    read_mode()
}

pub(crate) fn record(report: &Path, key: &str, mode: u32) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(report).unwrap();
    writeln!(file, "{key}={mode:x}").unwrap();
}

/// Appends a free-form line that the parent test shows in [`Child::log`].
pub(crate) fn note(report: &Path, line: &str) {
    use std::io::Write;
    let path = report.with_extension("notes");
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn start_tui() -> Tui {
    let terminal = ProcessTerminal::with_backend(ProcessTerminalBackend {
        env: Arc::new(MapEnv::new([("TERM", "xterm-256color")])),
        write: Box::new(|_| {}),
        size: Box::new(|| Some((80, 24))),
        real_io: true,
    });
    let options = TuiOptions { env: Some(Rc::new(MapEnv::new([("TERM", "xterm-256color")]))), ..Default::default() };
    let tui = Tui::new_main_screen(Rc::new(RefCell::new(terminal)), options);
    tui.start();
    tui
}

/// Records the mode right after the crash restore, before unwinding drops the TUI.
fn install_crash_report(report: PathBuf) {
    std::panic::set_hook(Box::new(move |_| {
        restore_terminal_after_crash();
        record(&report, "after", read_mode());
    }));
}

#[test]
#[ignore = "runs only as the child of the other windows_console tests"]
fn console_child() {
    let Ok(scenario) = std::env::var(SCENARIO) else { return };
    let report = PathBuf::from(std::env::var(REPORT).expect("report path"));
    let cooked = seed(read_mode() | COOKED | EXTENDED_FLAGS);
    let before = if scenario.starts_with("host-raw") { seed(cooked & !COOKED) } else { cooked };
    record(&report, "before", before);
    if scenario.ends_with("-crash") {
        if !scenario.contains("no-baseline") {
            save_terminal_mode_for_crash();
        }
        install_crash_report(report.clone());
    }
    match scenario.as_str() {
        "tui" | "host-raw-tui" => {
            let tui = start_tui();
            record(&report, "running", read_mode());
            tui.stop(TuiStopOptions { preserve_screen: true });
            record(&report, "after", read_mode());
        }
        "tui-crash" => {
            let _tui = start_tui();
            record(&report, "running", read_mode());
            panic!("crash while the TUI runs");
        }
        "extension-then-tui-crash" => {
            record(&report, "raw", seed(EXTENSION_RAW));
            let _tui = start_tui();
            panic!("crash while the TUI runs on extension raw mode");
        }
        "extension-crash" | "extension-no-baseline-crash" | "host-raw-extension-crash" => {
            record(&report, "raw", seed(EXTENSION_RAW));
            panic!("crash after extension raw mode");
        }
        "cooked-crash" => {
            record(&report, "changed", seed(before ^ QUICK_EDIT));
            panic!("crash with a cooked console");
        }
        "pi" => crate::windows_console_pi::run_pi(&report),
        other => panic!("unknown scenario {other}"),
    }
}

pub(crate) fn is_raw(mode: u32) -> bool {
    mode & COOKED == 0
}

fn tui_round_trip(backend: Backend) {
    let child = run("tui", backend);
    child.assert_exit(false);
    let running = child.mode("running");
    assert!(is_raw(running) && running & VIRTUAL_TERMINAL_INPUT != 0, "{running:x}\n{}", child.log);
    assert_eq!(child.mode("after"), child.mode("before"), "{}", child.log);

    let child = run("host-raw-tui", backend);
    child.assert_exit(false);
    assert!(is_raw(child.mode("before")), "{}", child.log);
    assert_eq!(child.mode("after"), child.mode("before"), "a host raw mode comes back exactly\n{}", child.log);
}

fn crash_restores(backend: Backend) {
    for scenario in ["tui-crash", "extension-crash", "extension-then-tui-crash", "host-raw-extension-crash"] {
        let child = run(scenario, backend);
        child.assert_exit(true);
        assert_eq!(child.mode("after"), child.mode("before"), "{}", child.log);
    }
    let child = run("extension-no-baseline-crash", backend);
    child.assert_exit(true);
    assert_eq!(child.mode("after"), child.mode("raw") | COOKED, "crossterm's fallback\n{}", child.log);

    let child = run("cooked-crash", backend);
    child.assert_exit(true);
    assert_eq!(child.mode("after"), child.mode("changed"), "unrelated bits stay\n{}", child.log);
}

#[test]
fn classic_console_raw_mode_round_trip() {
    tui_round_trip(Backend::Classic);
}

#[test]
fn classic_console_crash_restore() {
    crash_restores(Backend::Classic);
}

#[test]
#[ignore = "needs Windows PowerShell and ConPTY; run with --ignored"]
fn conpty_raw_mode_round_trip() {
    tui_round_trip(Backend::ConPty);
}

#[test]
#[ignore = "needs Windows PowerShell and ConPTY; run with --ignored"]
fn conpty_crash_restore() {
    crash_restores(Backend::ConPty);
}
