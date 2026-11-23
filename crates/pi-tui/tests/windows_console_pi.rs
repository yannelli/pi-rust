//! Starts the built `pi.exe` (`PI_TUI_TEST_PI`) in a classic or ConPTY console, quits it with
//! Ctrl+D, and checks the console input mode. Ignored; the console child runs [`run_pi`].

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Child as Process, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossterm_winapi::{Console, ConsoleMode, Handle, InputRecord, ScreenBuffer};

use crate::windows_console::{Backend, CONPTY_INPUT_DIR, COOKED, is_raw, note, read_mode, record, run};

const PI: &str = "PI_TUI_TEST_PI";
const PI_ARGS: [&str; 7] = [
    "--offline",
    "--no-approve",
    "--no-session",
    "--no-extensions",
    "--no-skills",
    "--no-prompt-templates",
    "--no-themes",
];
const CTRL_D: &str = "\u{4}";
const QUIT_COMMAND: &str = "/quit\r";
const WINDOW_INPUT: u32 = 0x0008;
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
const READY_DEADLINE: Duration = Duration::from_secs(40);
const EXIT_WAIT: Duration = Duration::from_secs(15);

fn console_file(name: &str) -> File {
    OpenOptions::new().read(true).write(true).open(name).unwrap_or_else(|error| panic!("open {name}: {error}"))
}

fn input_console() -> Console {
    Console::from(Handle::current_in_handle().expect("CONIN$"))
}

fn cursor() -> (i16, i16) {
    let info = ScreenBuffer::current().and_then(|buffer| buffer.info()).expect("console screen buffer");
    (info.cursor_pos().x, info.cursor_pos().y)
}

/// Types `keys` the way a terminal would: through the ConPTY input pipe in a ConPTY child, else
/// as virtual-key-0 records, which conhost passes through as text in VT input mode.
fn send_keys(keys: &str) {
    match std::env::var_os(CONPTY_INPUT_DIR) {
        Some(dir) => send_through_pipe(Path::new(&dir), keys),
        None => send_as_records(keys),
    }
}

/// Hands `keys` to `windows-conpty-run.ps1` and waits until it wrote them to the ConPTY input.
fn send_through_pipe(dir: &Path, keys: &str) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let name = format!("{:04}", NEXT.fetch_add(1, Ordering::SeqCst));
    let staged = dir.join(format!("{name}.tmp"));
    std::fs::write(&staged, keys).unwrap();
    std::fs::rename(&staged, dir.join(format!("{name}.keys"))).unwrap();
    let sent = dir.join(format!("{name}.sent"));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !sent.exists() {
        assert!(Instant::now() < deadline, "the ConPTY host did not write {keys:?} to the input pipe");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Writes `keys` to this process's console through a detached PowerShell, which leaves the console
/// modes alone.
fn send_as_records(keys: &str) {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/windows-console-keys.ps1");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .env("PI_CONSOLE_ATTACH_PID", std::process::id().to_string())
        .env("PI_CONSOLE_KEYS", keys)
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .output()
        .expect("run windows-console-keys.ps1");
    assert!(
        output.status.success(),
        "typing {keys:?} failed: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Reads every input record that arrives until 300 ms pass without a new one, for at most 3 s.
fn read_records(console: &Console) -> Vec<InputRecord> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut records = Vec::new();
    let mut last = Instant::now();
    while Instant::now() < deadline && (records.is_empty() || last.elapsed() < Duration::from_millis(300)) {
        if console.number_of_console_input_events().unwrap_or(0) > 0 {
            records.extend(console.read_console_input().expect("ReadConsoleInputW"));
            last = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    records
}

fn describe(records: &[InputRecord]) -> String {
    let mut text = String::new();
    for record in records {
        match record {
            InputRecord::KeyEvent(key) => {
                let direction = if key.key_down { "down" } else { "up" };
                let _ = write!(
                    text,
                    "[{direction} vk {:02x} char {:04x} {:?}]",
                    key.virtual_key_code, key.u_char, key.control_key_state
                );
            }
            other => {
                let _ = write!(text, "[{other:?}]");
            }
        }
    }
    text
}

/// Puts this console in pi-tui's raw input mode, types Ctrl+D, and checks that a pi-tui reader
/// would read it as 0x04. The modes come back afterward.
fn check_key_delivery(report: &Path) {
    let input_mode = ConsoleMode::from(Handle::current_in_handle().expect("CONIN$"));
    let output_mode = ConsoleMode::from(Handle::current_out_handle().expect("CONOUT$"));
    let (saved_input, saved_output) = (input_mode.mode().unwrap(), output_mode.mode().unwrap());
    input_mode.set_mode(saved_input & !COOKED | WINDOW_INPUT | VIRTUAL_TERMINAL_INPUT).unwrap();
    output_mode.set_mode(saved_output | VIRTUAL_TERMINAL_PROCESSING).unwrap();
    let console = input_console();
    let stale = read_records(&console);
    send_keys(CTRL_D);
    let records = read_records(&console);
    input_mode.set_mode(saved_input).unwrap();
    output_mode.set_mode(saved_output).unwrap();
    let line = format!("[key check] stale {}; after Ctrl+D: {}", describe(&stale), describe(&records));
    note(report, &line);
    let delivered =
        records.iter().any(|record| matches!(record, InputRecord::KeyEvent(key) if key.key_down && key.u_char == 4));
    assert!(delivered, "the console did not deliver Ctrl+D as 0x04: {line}");
}

fn wait_for_exit(child: &mut Process) -> Option<ExitStatus> {
    let deadline = Instant::now() + EXIT_WAIT;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn tail_since(path: &Path, from: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    let tail = String::from_utf8_lossy(bytes.get(from..).unwrap_or_default()).into_owned();
    tail.escape_debug().to_string()
}

/// Console child side: runs pi on this console and records `running`, `exit`, and `after`.
pub(crate) fn run_pi(report: &Path) {
    let pi = std::env::var_os(PI).unwrap_or_else(|| panic!("{PI} names the pi.exe to start"));
    check_key_delivery(report);
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let write_log = home.path().join("pi-output.log");
    let output = console_file("CONOUT$");
    let start_cursor = cursor();
    let mut child = Command::new(pi)
        .args(PI_ARGS)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join("agent"))
        .env("PI_OFFLINE", "1")
        .env("PI_TUI_WRITE_LOG", &write_log)
        .stdin(Stdio::from(console_file("CONIN$")))
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .spawn()
        .expect("start pi");

    // Ready: ProcessTerminal::start set raw mode and VT input, and the first frame moved the cursor.
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let mode = read_mode();
        if is_raw(mode) && mode & VIRTUAL_TERMINAL_INPUT != 0 && cursor() != start_cursor {
            record(report, "running", mode);
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("pi exited with {status:?} before its first frame; mode {mode:x}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("pi drew no raw-mode frame in {READY_DEADLINE:?}; mode {mode:x}, cursor {:?}", cursor());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let logged = std::fs::metadata(&write_log).map(|meta| meta.len() as usize).unwrap_or(0);
    send_keys(CTRL_D);
    if let Some(status) = wait_for_exit(&mut child) {
        record(report, "exit", status.code().unwrap_or(-1) as u32);
        record(report, "after", read_mode());
        return;
    }
    let pending = input_console().number_of_console_input_events().unwrap_or(u32::MAX);
    let mode = read_mode();
    let output = tail_since(&write_log, logged);
    send_keys(QUIT_COMMAND);
    let quit = wait_for_exit(&mut child);
    let _ = child.kill();
    panic!(
        "pi kept running {EXIT_WAIT:?} after Ctrl+D: {pending} unread input records, mode {mode:x}\n\
         pi output after Ctrl+D: {output}\n/quit then: {quit:?}, mode {:x}",
        read_mode()
    );
}

fn start_and_quit(backend: Backend) {
    assert!(std::env::var_os(PI).is_some(), "set {PI} to the absolute path of the built pi.exe");
    let child = run("pi", backend);
    for line in child.log.lines().filter(|line| line.starts_with("[key check]")) {
        eprintln!("{line}");
    }
    child.assert_exit(false);
    assert!(is_raw(child.mode("running")), "{}", child.log);
    assert_eq!(child.mode("exit"), 0, "pi exit code\n{}", child.log);
    assert_eq!(child.mode("after"), child.mode("before"), "{}", child.log);
}

#[test]
#[ignore = "needs PI_TUI_TEST_PI and Windows PowerShell; run with --ignored"]
fn classic_console_pi_start_and_quit() {
    start_and_quit(Backend::Classic);
}

#[test]
#[ignore = "needs PI_TUI_TEST_PI, Windows PowerShell, and ConPTY; run with --ignored"]
fn conpty_pi_start_and_quit() {
    start_and_quit(Backend::ConPty);
}
