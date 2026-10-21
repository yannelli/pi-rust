//! Run clipboard helper programs with a timeout (upstream `utils/clipboard-command.ts`).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(3000);
const MAX_BUFFER_BYTES: u64 = 50 * 1024 * 1024;

/// Runs a clipboard program. `None` means it failed; an empty output is a success.
pub trait ClipboardRunner: Sync {
    fn run(&self, command: &str, args: &[String], input: Option<&[u8]>, timeout: Duration) -> Option<Vec<u8>>;
}

/// Spawns real processes.
pub struct SystemClipboardRunner;

impl ClipboardRunner for SystemClipboardRunner {
    fn run(&self, command: &str, args: &[String], input: Option<&[u8]>, timeout: Duration) -> Option<Vec<u8>> {
        run_clipboard_command(command, args, input, timeout)
    }
}

/// Spawn `command`, feed `input` on stdin, and collect stdout when no input is given.
/// Writers get no output pipe because clipboard tools can daemonize and keep it open.
pub fn run_clipboard_command(
    command: &str,
    args: &[String],
    input: Option<&[u8]>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(if input.is_some() { Stdio::null() } else { Stdio::piped() })
        .stderr(Stdio::null());
    // Leave pi's process group so Ctrl+Z (SIGTSTP to the foreground group) does not stop a
    // helper that keeps serving the clipboard.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().ok()?;

    let stdin = child.stdin.take();
    let input = input.map(<[u8]>::to_vec);
    // A writer may exit before consuming all input; write errors are ignored.
    std::thread::spawn(move || {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            let _ = stdin.write_all(&input);
        }
    });
    let reader = child.stdout.take().map(|stdout| {
        std::thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = stdout.take(MAX_BUFFER_BYTES + 1).read_to_end(&mut buffer);
            result.ok().filter(|_| buffer.len() as u64 <= MAX_BUFFER_BYTES).map(|_| buffer)
        })
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let output = match reader {
        Some(reader) => reader.join().ok().flatten()?,
        None => Vec::new(),
    };
    status.success().then_some(output)
}
