//! Stop signal handling (upstream `utils/shell.ts` detached child tracking and the
//! SIGTERM/SIGHUP handlers of print and interactive mode). Shell commands run in their own
//! process group, so a signal to pi does not reach them; they are killed here before exit.

use std::collections::BTreeSet;
use std::sync::Mutex;

static TRACKED: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

fn tracked() -> std::sync::MutexGuard<'static, BTreeSet<u32>> {
    TRACKED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remember a detached child so [`kill_tracked_detached_children`] can kill it.
pub fn track_detached_child(pid: u32) {
    tracked().insert(pid);
}

pub fn untrack_detached_child(pid: u32) {
    tracked().remove(&pid);
}

/// The pids currently tracked.
pub fn tracked_detached_children() -> Vec<u32> {
    tracked().iter().copied().collect()
}

/// Tracks `pid` until dropped.
#[derive(Debug)]
pub struct DetachedChildGuard(Option<u32>);

impl DetachedChildGuard {
    pub fn new(pid: Option<u32>) -> Self {
        if let Some(pid) = pid {
            track_detached_child(pid);
        }
        Self(pid)
    }
}

impl Drop for DetachedChildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            untrack_detached_child(pid);
        }
    }
}

/// Kill every tracked child with its process tree, then forget them.
pub fn kill_tracked_detached_children() {
    let pids = std::mem::take(&mut *tracked());
    for pid in pids {
        kill_process_tree(pid);
    }
}

/// SIGKILL the process group of `pid`, or the process itself when that fails. On macOS the
/// group is stopped first.
#[cfg(unix)]
pub fn kill_process_tree(pid: u32) {
    use rustix::process::{Pid, Signal, kill_process, kill_process_group};
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else { return };
    kill_tree_steps(cfg!(target_os = "macos"), |step| match step {
        KillStep::StopGroup => kill_process_group(pid, Signal::STOP).is_ok(),
        KillStep::KillGroup => kill_process_group(pid, Signal::KILL).is_ok(),
        KillStep::KillProcess => kill_process(pid, Signal::KILL).is_ok(),
    });
}

/// One signal [`kill_process_tree`] sends.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillStep {
    StopGroup,
    KillGroup,
    KillProcess,
}

/// Runs the kill steps through `send`, which reports whether the signal was delivered.
#[cfg(unix)]
fn kill_tree_steps(stop_group_first: bool, mut send: impl FnMut(KillStep) -> bool) {
    if stop_group_first {
        // macOS signals group members one by one, so a killed child can wake its shell before
        // the shell is signalled and `a; b` starts b (upstream #11252). Stopping first narrows
        // that race; SIGKILL also kills stopped processes. A failed stop must not skip the kill.
        send(KillStep::StopGroup);
    }
    if !send(KillStep::KillGroup) {
        send(KillStep::KillProcess);
    }
}

/// `taskkill /F /T` from System32, without waiting for it.
#[cfg(windows)]
pub fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let taskkill = crate::platform::shell::taskkill_exe(std::env::var_os("SystemRoot"));
    let _ = std::process::Command::new(taskkill)
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(any(unix, windows)))]
pub fn kill_process_tree(_pid: u32) {}

/// Exit codes after a stop signal: 143 for SIGTERM, 129 for SIGHUP.
pub const SIGTERM_EXIT_CODE: i32 = 143;
pub const SIGHUP_EXIT_CODE: i32 = 129;

/// Wait for SIGTERM (143) or SIGHUP (129). Never resolves when the handlers cannot be installed.
#[cfg(unix)]
pub async fn termination_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut hangup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => SIGTERM_EXIT_CODE,
        _ = hangup.recv() => SIGHUP_EXIT_CODE,
    }
}

/// Wait for Ctrl+Break or the console window closing, the Windows counterparts of SIGTERM.
#[cfg(windows)]
pub async fn termination_signal() -> i32 {
    use tokio::signal::windows::{ctrl_break, ctrl_close};
    let (Ok(mut brk), Ok(mut close)) = (ctrl_break(), ctrl_close()) else {
        return std::future::pending().await;
    };
    tokio::select! {
        _ = brk.recv() => SIGTERM_EXIT_CODE,
        _ = close.recv() => SIGHUP_EXIT_CODE,
    }
}

#[cfg(not(any(unix, windows)))]
pub async fn termination_signal() -> i32 {
    std::future::pending().await
}

pub use pi_tui::process_terminal::is_dead_terminal_error;

/// Exit at once after the terminal died: kill tracked children and skip all cleanup, which
/// would write restore sequences to the dead terminal (upstream `emergencyTerminalExit`).
pub fn emergency_terminal_exit() -> ! {
    kill_tracked_detached_children();
    std::process::exit(SIGHUP_EXIT_CODE)
}

/// Exit through [`emergency_terminal_exit`] when terminal reads or writes fail with a dead
/// terminal error, as upstream does for the interactive mode.
pub fn install_dead_terminal_exit() {
    fn exit_on_dead_terminal() {
        emergency_terminal_exit()
    }
    pi_tui::process_terminal::set_dead_terminal_handler(Some(exit_on_dead_terminal));
}

#[cfg(test)]
mod tests {
    use super::*;

    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn guards_track_until_dropped() {
        let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let guard = DetachedChildGuard::new(Some(4_000_001));
        assert!(tracked_detached_children().contains(&4_000_001));
        drop(guard);
        assert!(!tracked_detached_children().contains(&4_000_001));
        let _none = DetachedChildGuard::new(None);
    }

    #[test]
    fn classifies_dead_terminal_errors() {
        assert!(is_dead_terminal_error(&std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
        assert!(is_dead_terminal_error(&std::io::Error::from(std::io::ErrorKind::NotConnected)));
        assert!(!is_dead_terminal_error(&std::io::Error::from(std::io::ErrorKind::NotFound)));
        #[cfg(unix)]
        {
            let eio = rustix::io::Errno::IO.raw_os_error();
            assert!(is_dead_terminal_error(&std::io::Error::from_raw_os_error(eio)));
            let enotty = rustix::io::Errno::NOTTY.raw_os_error();
            assert!(is_dead_terminal_error(&std::io::Error::from_raw_os_error(enotty)));
        }
    }

    #[cfg(unix)]
    fn kill_steps(stop_group_first: bool, delivered: impl Fn(KillStep) -> bool) -> Vec<KillStep> {
        let mut sent = Vec::new();
        kill_tree_steps(stop_group_first, |step| {
            sent.push(step);
            delivered(step)
        });
        sent
    }

    #[cfg(unix)]
    #[test]
    fn stops_the_macos_group_before_killing_it() {
        use KillStep::{KillGroup, KillProcess, StopGroup};
        assert_eq!(kill_steps(true, |_| true), [StopGroup, KillGroup]);
        assert_eq!(kill_steps(true, |step| step != StopGroup), [StopGroup, KillGroup]);
        assert_eq!(kill_steps(true, |step| step != KillGroup), [StopGroup, KillGroup, KillProcess]);
        assert_eq!(kill_steps(true, |_| false), [StopGroup, KillGroup, KillProcess]);
        // Linux holds the task list lock across group signalling, so it keeps the plain kill.
        assert_eq!(kill_steps(false, |_| true), [KillGroup]);
    }

    #[cfg(unix)]
    #[test]
    fn kills_tracked_process_groups() {
        let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        track_detached_child(child.id());
        kill_tracked_detached_children();
        let status = child.wait().unwrap();
        assert!(!status.success());
        assert!(!tracked_detached_children().contains(&child.id()));
    }
}
