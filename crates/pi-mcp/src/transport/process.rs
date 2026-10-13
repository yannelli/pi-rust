//! Process groups of stdio servers: stopping a server with its children, and command lookup.

use std::collections::HashSet;
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopSignal {
    Term,
    Kill,
}

/// Process groups of running servers, stopped by `kill_live_process_groups` when the host exits.
static LIVE_GROUPS: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

fn live_groups() -> MutexGuard<'static, Option<HashSet<u32>>> {
    LIVE_GROUPS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn track(pid: u32) {
    live_groups().get_or_insert_with(HashSet::new).insert(pid);
}

pub(crate) fn untrack(pid: u32) {
    if let Some(groups) = live_groups().as_mut() {
        groups.remove(&pid);
    }
}

/// Send SIGTERM to the process groups of stdio servers that were not closed. Call it before the host
/// exits, so wrappers like `npx` or `uvx` do not leave servers behind.
pub fn kill_live_process_groups() {
    let groups: Vec<u32> = live_groups().as_ref().map(|groups| groups.iter().copied().collect()).unwrap_or_default();
    for pid in groups {
        kill_tree(pid, StopSignal::Term);
    }
}

/// Signal the server's whole process group, falling back to the process itself.
#[cfg(unix)]
pub(crate) fn kill_tree(pid: u32, signal: StopSignal) {
    use rustix::process::{Pid, Signal, kill_process, kill_process_group};
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else { return };
    let signal = match signal {
        StopSignal::Term => Signal::TERM,
        StopSignal::Kill => Signal::KILL,
    };
    if kill_process_group(pid, signal).is_err() {
        let _ = kill_process(pid, signal);
    }
}

/// Windows has no graceful signals, and `.cmd` shims run through cmd.exe, so the whole tree is killed.
#[cfg(windows)]
pub(crate) fn kill_tree(pid: u32, _signal: StopSignal) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/T", "/F"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn kill_tree(_pid: u32, _signal: StopSignal) {}

/// Find a bare command in `PATH`, trying `PATHEXT` extensions, so `.cmd` shims like `npx.cmd` run.
#[cfg(windows)]
pub(crate) fn resolve_command(command: &str, env_path: Option<&str>) -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    let path = Path::new(command);
    if path.components().count() > 1 || path.extension().is_some() {
        return PathBuf::from(command);
    }
    let path_var = env_path.map(str::to_string).or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    for dir in std::env::split_paths(&path_var) {
        for extension in extensions.split(';').filter(|extension| !extension.is_empty()) {
            let candidate = dir.join(format!("{command}{}", extension.to_ascii_lowercase()));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(command)
}

#[cfg(not(windows))]
pub(crate) fn resolve_command(command: &str, _env_path: Option<&str>) -> std::path::PathBuf {
    std::path::PathBuf::from(command)
}

/// Node-style spawn error text, like `spawn missing-server ENOENT`.
pub(crate) fn spawn_error(command: &str, error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("spawn {command} ENOENT"),
        std::io::ErrorKind::PermissionDenied => format!("spawn {command} EACCES"),
        _ => format!("spawn {command}: {error}"),
    }
}
