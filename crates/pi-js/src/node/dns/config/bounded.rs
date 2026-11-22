//! Size and time limits for the files and system utilities that resolver settings come from.

use std::io::Read;
use std::path::Path;
#[cfg(any(windows, target_os = "macos", test))]
use std::process::{Command, Stdio};
#[cfg(any(windows, target_os = "macos", test))]
use std::time::Duration;

/// How long `scutil`, `netsh`, or `reg` may run before it is killed.
#[cfg(any(windows, target_os = "macos", test))]
pub(in crate::node::dns) const HELPER_DEADLINE: Duration = Duration::from_secs(2);
/// A killed child exits at once; this bounds the wait for a process the kernel holds.
#[cfg(any(windows, target_os = "macos", test))]
const REAP_DEADLINE: Duration = Duration::from_secs(1);
#[cfg(any(windows, target_os = "macos"))]
pub(in crate::node::dns) const MAX_HELPER_OUTPUT: usize = 4 << 20;
#[cfg(not(windows))]
pub(in crate::node::dns) const MAX_CONFIG_FILE: usize = 1 << 20;

#[cfg(any(windows, target_os = "macos", test))]
pub(in crate::node::dns) struct Output {
    pub success: bool,
    pub stdout: Vec<u8>,
}

/// Up to `limit` bytes of a regular file, cut after the last full line when the file is longer.
/// Devices, FIFOs, and directories give `None`, so a read cannot block on them.
pub(in crate::node::dns) fn read(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    #[cfg(not(unix))]
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let file = options.open(path).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() > limit {
        let end = bytes[..limit].iter().rposition(|byte| *byte == b'\n').map_or(0, |at| at + 1);
        bytes.truncate(end);
    }
    Some(bytes)
}

/// Runs on a discovery thread with its own reactor. Returns `None` on setup, timeout or output errors.
/// On failure it closes stdout, kills the child and waits up to `REAP_DEADLINE` for it to exit.
#[cfg(any(windows, target_os = "macos", test))]
pub(in crate::node::dns) fn run(command: Command, deadline: Duration, limit: usize) -> Option<Output> {
    use tokio::io::AsyncReadExt;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    let mut command = tokio::process::Command::from(command);
    command.stdin(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    runtime.block_on(async move {
        let end = tokio::time::Instant::now() + deadline;
        #[cfg(windows)]
        let (mut child, stdout) = {
            let (server, writer) = super::bounded_windows::pipe().ok()?;
            // Connect while this process holds the writer, so the helper cannot exit first.
            tokio::time::timeout_at(end, server.connect()).await.ok()?.ok()?;
            command.stdout(writer);
            let child = command.spawn().ok()?;
            // The command holds the parent's copy of the write end, which would delay EOF.
            drop(command);
            (child, Some(server))
        };
        #[cfg(not(windows))]
        let (mut child, stdout) = {
            let mut child = command.stdout(Stdio::piped()).spawn().ok()?;
            let stdout = child.stdout.take();
            (child, stdout)
        };
        let output = tokio::time::timeout_at(end, async {
            let mut stdout = stdout?;
            let mut bytes = Vec::new();
            (&mut stdout).take(limit as u64 + 1).read_to_end(&mut bytes).await.ok()?;
            if bytes.len() > limit {
                return None;
            }
            let status = child.wait().await.ok()?;
            Some(Output { success: status.success(), stdout: bytes })
        })
        .await
        .ok()
        .flatten();
        if output.is_none() {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(REAP_DEADLINE, child.wait()).await;
        }
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::time::Instant;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pi-dns-bounded-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_regular_files_up_to_the_last_full_line_within_the_limit() {
        let dir = temp_dir("read");
        let file = dir.join("hosts");
        std::fs::write(&file, "10.0.0.1 a\n10.0.0.2 b\n10.0.0.3 c\n").unwrap();
        assert_eq!(read(&file, 64).unwrap(), b"10.0.0.1 a\n10.0.0.2 b\n10.0.0.3 c\n");
        assert_eq!(read(&file, 25).unwrap(), b"10.0.0.1 a\n10.0.0.2 b\n");
        assert_eq!(read(&file, 5).unwrap(), b"");
        assert!(read(&dir, 64).is_none() && read(&dir.join("missing"), 64).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fifos_and_devices_are_refused_without_blocking() {
        let dir = temp_dir("fifo");
        let fifo = dir.join("resolv.conf");
        assert!(Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        assert!(read(&fifo, 64).is_none());
        assert!(read(Path::new("/dev/zero"), 64).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    fn shell(script: &str, deadline: Duration, limit: usize) -> Option<Output> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        run(command, deadline, limit)
    }

    #[cfg(unix)]
    #[test]
    fn returns_output_and_exit_status() {
        let output = shell("printf 'DNS configuration\\n'", HELPER_DEADLINE, 64).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout, b"DNS configuration\n");
        let output = shell("printf partial; exit 3", HELPER_DEADLINE, 64).unwrap();
        assert_eq!((output.success, output.stdout.as_slice()), (false, b"partial".as_slice()));
        assert!(run(Command::new("/nonexistent/pi-dns-helper"), HELPER_DEADLINE, 64).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn oversized_output_is_refused() {
        assert!(shell("printf %0100d 0", HELPER_DEADLINE, 99).is_none());
        assert_eq!(shell("printf %0100d 0", HELPER_DEADLINE, 100).unwrap().stdout.len(), 100);
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_helper_is_killed_and_reaped_at_the_deadline() {
        let dir = temp_dir("stall");
        let pid_file = dir.join("pid");
        let script = format!("echo $$ > '{}'; printf started; exec sleep 30", pid_file.display());
        let started = Instant::now();
        assert!(shell(&script, Duration::from_millis(200), 64).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill").args(["-0", pid.trim()]).stderr(Stdio::null()).status().unwrap();
        assert!(!alive.success(), "helper {pid} was not reaped");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_held_open_by_a_grandchild_stops_at_the_deadline() {
        let started = Instant::now();
        assert!(shell("sleep 3 & printf early", Duration::from_millis(200), 64).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
