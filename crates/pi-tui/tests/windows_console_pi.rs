//! Starts the built `pi.exe` (`PI_TUI_TEST_PI`) in a real console and quits it normally: Ctrl+D
//! through the ConPTY input pipe, or a test extension's `ctx.shutdown()` in the classic console.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm_winapi::ScreenBuffer;

use crate::windows_console::{Backend, CONPTY_INPUT_DIR, is_raw, read_mode, record, run};

const PI: &str = "PI_TUI_TEST_PI";
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const READY_DEADLINE: Duration = Duration::from_secs(40);
const EXIT_DEADLINE: Duration = Duration::from_secs(15);
/// Shuts pi down once the file in `PI_TEST_REQUEST_QUIT` exists.
const QUIT_EXTENSION: &str = r#"import { existsSync } from "node:fs";
export default function (pi) {
  pi.on("session_start", (_event, ctx) => {
    const timer = setInterval(() => {
      if (!existsSync(process.env.PI_TEST_REQUEST_QUIT)) return;
      clearInterval(timer);
      ctx.shutdown();
    }, 25);
  });
}
"#;

fn console_file(name: &str) -> File {
    OpenOptions::new().read(true).write(true).open(name).unwrap_or_else(|error| panic!("open {name}: {error}"))
}

fn cursor() -> (i16, i16) {
    let info = ScreenBuffer::current().and_then(|buffer| buffer.info()).expect("console screen buffer");
    (info.cursor_pos().x, info.cursor_pos().y)
}

/// Hands Ctrl+D to `windows-conpty-run.ps1` and waits until it wrote it to the ConPTY input.
fn send_ctrl_d_through_conpty(dir: &Path) {
    std::fs::write(dir.join("ctrl-d.tmp"), "\u{4}").unwrap();
    std::fs::rename(dir.join("ctrl-d.tmp"), dir.join("ctrl-d.keys")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.join("ctrl-d.sent").exists() {
        assert!(Instant::now() < deadline, "the ConPTY host did not write Ctrl+D to the input pipe");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Console child side: runs pi on this console and records `running`, `exit`, and `after`.
pub(crate) fn run_pi(report: &Path) {
    let pi = std::env::var_os(PI).unwrap_or_else(|| panic!("{PI} names the pi.exe to start"));
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let quit_request = home.path().join("quit");
    let write_log = home.path().join("pi-output.log");
    let conpty_input = std::env::var_os(CONPTY_INPUT_DIR);
    let output = console_file("CONOUT$");
    let start_cursor = cursor();
    let mut command = Command::new(pi);
    command.args(["--offline", "--no-approve", "--no-session", "--no-extensions"]);
    if conpty_input.is_none() {
        let extension = home.path().join("test-quit.mjs");
        std::fs::write(&extension, QUIT_EXTENSION).unwrap();
        command.arg("--extension").arg(extension).env("PI_TEST_REQUEST_QUIT", &quit_request);
    }
    let mut child = command
        .args(["--no-skills", "--no-prompt-templates", "--no-themes"])
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

    let logged = std::fs::metadata(&write_log).map_or(0, |meta| meta.len() as usize);
    match &conpty_input {
        Some(dir) => send_ctrl_d_through_conpty(Path::new(dir)),
        None => std::fs::write(&quit_request, "").unwrap(),
    }
    let deadline = Instant::now() + EXIT_DEADLINE;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            record(report, "exit", status.code().unwrap_or(-1) as u32);
            record(report, "after", read_mode());
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let bytes = std::fs::read(&write_log).unwrap_or_default();
    let tail = String::from_utf8_lossy(bytes.get(logged..).unwrap_or_default()).escape_debug().to_string();
    panic!("pi kept running {EXIT_DEADLINE:?} after the quit request; mode {:x}\npi output since: {tail}", read_mode());
}

fn start_and_quit(backend: Backend) {
    assert!(std::env::var_os(PI).is_some(), "set {PI} to the absolute path of the built pi.exe");
    let child = run("pi", backend);
    child.assert_exit(false);
    assert!(is_raw(child.mode("running")), "{}", child.log);
    assert_eq!(child.mode("exit"), 0, "pi exit code\n{}", child.log);
    assert_eq!(child.mode("after"), child.mode("before"), "{}", child.log);
}

#[test]
#[ignore = "needs PI_TUI_TEST_PI; run with --ignored"]
fn classic_console_pi_start_and_quit() {
    start_and_quit(Backend::Classic);
}

#[test]
#[ignore = "needs PI_TUI_TEST_PI, Windows PowerShell, and ConPTY; run with --ignored"]
fn conpty_pi_start_and_quit() {
    start_and_quit(Backend::ConPty);
}
