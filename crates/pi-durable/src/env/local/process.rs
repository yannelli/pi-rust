//! Shell resolution and process-tree kills of the local environment.

use std::path::Path;

use crate::env::{ExecutionError, ExecutionErrorCode};

#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A shell program, the arguments before the command, and whether the command goes on standard input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
    pub command_from_stdin: bool,
}

fn is_legacy_wsl_bash_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_lowercase();
    let Some(rest) = normalized.get(1..) else { return false };
    let drive = normalized.as_bytes()[0];
    drive.is_ascii_lowercase()
        && (rest == ":\\windows\\system32\\bash.exe" || rest == ":\\windows\\sysnative\\bash.exe")
}

fn bash_shell_config(shell: String) -> ShellConfig {
    if is_legacy_wsl_bash_path(&shell) {
        ShellConfig { shell, args: vec!["-s".into()], command_from_stdin: true }
    } else {
        ShellConfig { shell, args: vec!["-c".into()], command_from_stdin: false }
    }
}

/// The first `name` on `PATH` (upstream runs `which bash` or `where bash.exe`).
fn find_on_path(name: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

/// The shell for string commands: `custom` when given, else Git Bash or `bash.exe` on `PATH` on Windows, and
/// `/bin/bash`, `bash` on `PATH`, or `sh` elsewhere.
pub(crate) fn shell_config(custom: Option<&str>) -> Result<ShellConfig, ExecutionError> {
    if let Some(custom) = custom.filter(|custom| !custom.is_empty()) {
        if Path::new(custom).exists() {
            return Ok(bash_shell_config(custom.to_string()));
        }
        let message = format!("Custom shell path not found: {custom}");
        return Err(ExecutionError::new(ExecutionErrorCode::ShellUnavailable, message));
    }
    if cfg!(windows) {
        let candidates: Vec<String> = ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .map(|root| format!("{root}\\Git\\bin\\bash.exe"))
            .collect();
        if let Some(found) = candidates.iter().find(|candidate| Path::new(candidate).exists()) {
            return Ok(bash_shell_config(found.clone()));
        }
        if let Some(found) = find_on_path("bash.exe") {
            return Ok(bash_shell_config(found));
        }
        let searched: Vec<String> = candidates.iter().map(|path| format!("  {path}")).collect();
        let message = format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Configure an explicit shellPath\n\nSearched Git Bash in:\n{}",
            searched.join("\n")
        );
        return Err(ExecutionError::new(ExecutionErrorCode::ShellUnavailable, message));
    }
    if Path::new("/bin/bash").exists() {
        return Ok(bash_shell_config("/bin/bash".into()));
    }
    if let Some(found) = find_on_path("bash") {
        return Ok(bash_shell_config(found));
    }
    Ok(ShellConfig { shell: "sh".into(), args: vec!["-c".into()], command_from_stdin: false })
}

/// Kill a process and its descendants: its process group on Unix, `taskkill /F /T` on Windows. Best effort.
#[cfg(unix)]
pub(crate) fn kill_process_tree(pid: u32) {
    use rustix::process::{Pid, Signal, kill_process, kill_process_group};
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else { return };
    if kill_process_group(pid, Signal::KILL).is_err() {
        let _ = kill_process(pid, Signal::KILL);
    }
}

/// Kill a process and its descendants: its process group on Unix, `taskkill /F /T` on Windows. Best effort.
#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let _ = std::process::Command::new(root.join("System32").join("taskkill.exe"))
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// The exit code of a finished process; a signal is `128 + signal`.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}
