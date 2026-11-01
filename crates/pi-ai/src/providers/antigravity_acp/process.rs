//! Finding and starting the Antigravity ACP server that the user installed.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::providers::acp::connection::Connection;

const BINARY_NAMES: [&str; 2] = ["agy_acp_server.par", "agy_acp_server.exe"];
const LOOPBACK: &str = "localhost,127.0.0.1,::1";
/// The server prints this line, then the Google sign-in URL, to stderr.
const SIGN_IN_PREFIX: &str = "Open the following link to authenticate the ACP server: ";

pub(crate) const NOT_INSTALLED: &str = "The Antigravity ACP server is not installed. Run /login google-antigravity to install it, or set PI_ANTIGRAVITY_ACP_SERVER to the path of agy_acp_server.";

fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

/// `PI_ANTIGRAVITY_ACP_SERVER`, `AGY_ACP_BIN`, the install locations of Google's setup, then `PATH`.
pub(crate) fn locate(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = ["PI_ANTIGRAVITY_ACP_SERVER", "AGY_ACP_BIN"]
        .iter()
        .filter_map(|name| env(name).filter(|value| !value.trim().is_empty()))
        .map(|value| expand_home(value.trim()))
        .collect();
    if let Some(home) = env("HOME") {
        let home = PathBuf::from(home);
        candidates.extend(BINARY_NAMES.iter().map(|name| home.join(".local/opt/agy-acp/current").join(name)));
        candidates.push(home.join(".local/bin").join(BINARY_NAMES[0]));
    }
    for directory in env("PATH").iter().flat_map(|path| std::env::split_paths(path).collect::<Vec<_>>()) {
        candidates.extend(BINARY_NAMES.iter().map(|name| directory.join(name)));
    }
    candidates.into_iter().find(|candidate| executable(candidate))
}

/// Google's Linux build without a `#!` line takes `--uid=`.
fn arguments(binary: &Path) -> Vec<String> {
    let script = std::fs::read(binary).map(|bytes| bytes.starts_with(b"#!")).unwrap_or(false);
    if cfg!(target_os = "linux") && !script { vec!["--uid=".into()] } else { Vec::new() }
}

/// A running server; dropping it stops the process.
pub(crate) struct Server {
    pub connection: Arc<Connection>,
    /// The Google sign-in URLs that the server prints to stderr.
    pub sign_in: mpsc::UnboundedReceiver<String>,
    child: Child,
}

impl Server {
    /// Stops the server and the processes it started, and waits for it to exit.
    pub(crate) async fn shutdown(&mut self) {
        crate::providers::acp::process::kill_group(&self.child);
        let _ = self.child.kill().await;
    }
}

/// A server dropped without `shutdown`, for example by a cancelled start, still takes its processes along.
impl Drop for Server {
    fn drop(&mut self) {
        crate::providers::acp::process::kill_group(&self.child);
    }
}

pub(crate) fn sign_in_url(line: &str) -> Option<String> {
    let url = line.split_once(SIGN_IN_PREFIX)?.1.split_whitespace().next()?;
    url.starts_with("https://").then(|| url.to_string())
}

pub(crate) fn spawn(binary: &Path, cwd: &Path) -> std::io::Result<Server> {
    let no_proxy = |name: &str| match std::env::var(name) {
        Ok(existing) if !existing.is_empty() => format!("{existing},{LOOPBACK}"),
        _ => LOOPBACK.to_string(),
    };
    let mut command = Command::new(binary);
    command
        .args(arguments(binary))
        .current_dir(cwd)
        .env("NO_PROXY", no_proxy("NO_PROXY"))
        .env("no_proxy", no_proxy("no_proxy"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| std::io::Error::other("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| std::io::Error::other("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| std::io::Error::other("no stderr"))?;
    let (urls, sign_in) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(url) = sign_in_url(&line) {
                let _ = urls.send(url);
            }
        }
    });
    Ok(Server { connection: Connection::new(super::LABEL, stdout, stdin), sign_in, child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_override_wins_and_missing_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("agy_acp_server.par");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = dir.path().to_string_lossy().to_string();
        let from_path = |name: &str| (name == "PATH").then(|| path.clone());
        assert_eq!(locate(&from_path), Some(binary.clone()));
        let override_missing = |name: &str| match name {
            "PI_ANTIGRAVITY_ACP_SERVER" => Some("/nonexistent/agy".to_string()),
            "PATH" => Some(path.clone()),
            _ => None,
        };
        assert_eq!(locate(&override_missing), Some(binary.clone()));
        assert!(arguments(&binary).is_empty());
        assert_eq!(locate(&|_: &str| None), None);
    }

    #[test]
    fn reads_the_sign_in_url_from_the_stderr_line() {
        let line = format!("{SIGN_IN_PREFIX}https://accounts.google.com/o/oauth2/auth?client_id=x");
        assert_eq!(sign_in_url(&line).as_deref(), Some("https://accounts.google.com/o/oauth2/auth?client_id=x"));
        assert_eq!(sign_in_url("INFO Could not open a browser automatically"), None);
    }

    /// The server and a process it started have both ended, and the server is reaped, after `shutdown`.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_stops_the_server_and_its_process_group() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("agy_acp_server");
        std::fs::write(&binary, "#!/bin/sh\nsleep 300 &\necho $! > grandchild.pid\nwait\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut server = spawn(&binary, dir.path()).unwrap();
        let leader = i32::try_from(server.child.id().unwrap()).unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let grandchild = loop {
            let pid = std::fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<i32>().ok());
            if let Some(pid) = pid.filter(|pid| *pid > 0) {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline, "the fake server did not start its child");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        server.shutdown().await;
        let alive = |pid: i32| {
            rustix::process::Pid::from_raw(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        };
        assert!(!alive(leader), "the server is reaped when shutdown returns");
        while alive(grandchild) {
            assert!(tokio::time::Instant::now() < deadline, "the server's child outlived shutdown");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A server dropped mid-start, as a timed-out open drops it, still stops the processes it started.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_server_stops_its_process_group() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("agy_acp_server");
        std::fs::write(&binary, "#!/bin/sh\nsleep 300 &\necho $! > grandchild.pid\nwait\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        let server = spawn(&binary, dir.path()).unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let grandchild = loop {
            let pid = std::fs::read_to_string(&pid_file).ok().and_then(|text| text.trim().parse::<i32>().ok());
            if let Some(pid) = pid.filter(|pid| *pid > 0) {
                break pid;
            }
            assert!(tokio::time::Instant::now() < deadline, "the fake server did not start its child");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        drop(server);
        let alive = |pid: i32| {
            rustix::process::Pid::from_raw(pid).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        };
        while alive(grandchild) {
            assert!(tokio::time::Instant::now() < deadline, "the server's child outlived the dropped server");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
