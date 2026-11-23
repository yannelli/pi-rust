//! Starts the built `pi.exe` (`PI_TUI_TEST_PI`) in a classic or ConPTY console, quits it with
//! Ctrl+D, and checks the console input mode. Ignored; the console child runs [`run_pi`].

use std::fs::{File, OpenOptions};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crossterm_winapi::ScreenBuffer;

use crate::windows_console::{Backend, is_raw, read_mode, record, run};

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
const VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
const READY_DEADLINE: Duration = Duration::from_secs(40);
const EXIT_WAIT: Duration = Duration::from_secs(5);
const CTRL_D_ATTEMPTS: usize = 4;

fn cursor() -> (i16, i16) {
    let info = ScreenBuffer::current().and_then(|buffer| buffer.info()).expect("console screen buffer");
    (info.cursor_pos().x, info.cursor_pos().y)
}

fn console_file(name: &str) -> File {
    OpenOptions::new().read(true).write(true).open(name).unwrap_or_else(|error| panic!("open {name}: {error}"))
}

/// Writes Ctrl+D key records to this process's console through a detached PowerShell, which
/// leaves the console modes alone.
fn send_ctrl_d() {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/windows-console-ctrl-d.ps1");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .env("PI_CONSOLE_ATTACH_PID", std::process::id().to_string())
        .creation_flags(DETACHED_PROCESS)
        .stdin(Stdio::null())
        .output()
        .expect("run windows-console-ctrl-d.ps1");
    assert!(
        output.status.success(),
        "Ctrl+D injection failed: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Console child side: runs pi on this console and records `running`, `exit`, and `after`.
pub(crate) fn run_pi(report: &Path) {
    let pi = std::env::var_os(PI).unwrap_or_else(|| panic!("{PI} names the pi.exe to start"));
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let output = console_file("CONOUT$");
    let start_cursor = cursor();
    let mut child = Command::new(pi)
        .args(PI_ARGS)
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("PI_CODING_AGENT_DIR", home.path().join("agent"))
        .env("PI_OFFLINE", "1")
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

    let mut status = None;
    for _ in 0..CTRL_D_ATTEMPTS {
        send_ctrl_d();
        let wait_until = Instant::now() + EXIT_WAIT;
        while status.is_none() && Instant::now() < wait_until {
            status = child.try_wait().unwrap();
            std::thread::sleep(Duration::from_millis(50));
        }
        if status.is_some() {
            break;
        }
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("pi kept running after {CTRL_D_ATTEMPTS} Ctrl+D presses; mode {:x}", read_mode());
    };
    record(report, "exit", status.code().unwrap_or(-1) as u32);
    record(report, "after", read_mode());
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
#[ignore = "needs PI_TUI_TEST_PI and Windows PowerShell; run with --ignored"]
fn classic_console_pi_start_and_quit() {
    start_and_quit(Backend::Classic);
}

#[test]
#[ignore = "needs PI_TUI_TEST_PI, Windows PowerShell, and ConPTY; run with --ignored"]
fn conpty_pi_start_and_quit() {
    start_and_quit(Backend::ConPty);
}
