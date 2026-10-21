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

/// Node's `path.win32.normalize` for drive and rooted paths: `/` becomes `\`, empty and `.`
/// segments drop, and `..` removes the previous segment.
fn win32_normalize(path: &str) -> String {
    let path = path.replace('/', "\\");
    let bytes = path.as_bytes();
    let drive = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' { &path[..2] } else { "" };
    let after_drive = &path[drive.len()..];
    let rooted = after_drive.starts_with('\\');
    let mut segments: Vec<&str> = Vec::new();
    for segment in after_drive.split('\\') {
        match segment {
            "" | "." => {}
            ".." => match segments.last() {
                Some(&last) if last != ".." => {
                    segments.pop();
                }
                _ if rooted => {}
                _ => segments.push(".."),
            },
            _ => segments.push(segment),
        }
    }
    let mut tail = segments.join("\\");
    if tail.is_empty() {
        if !rooted {
            tail.push('.');
        }
    } else if after_drive.ends_with('\\') {
        tail.push('\\');
    }
    format!("{drive}{}{tail}", if rooted { "\\" } else { "" })
}

/// The legacy WSL launcher, `bash.exe` under `System32` or `Sysnative` of `system_root`, reads
/// commands from stdin. Without `system_root` only the conventional `C:\Windows` paths count.
fn is_legacy_wsl_bash_path(path: &str, system_root: Option<&str>) -> bool {
    let normalized = win32_normalize(path).to_lowercase();
    if let Some(root) = system_root.filter(|root| !root.is_empty()) {
        if !matches!(root.as_bytes(), [letter, b':', b'\\' | b'/', ..] if letter.is_ascii_alphabetic()) {
            return false;
        }
        return ["System32", "Sysnative"]
            .iter()
            .any(|directory| normalized == win32_normalize(&format!("{root}\\{directory}\\bash.exe")).to_lowercase());
    }
    matches!(
        normalized.as_bytes(),
        [b'a'..=b'z', b':', b'\\', rest @ ..]
            if rest == b"windows\\system32\\bash.exe" || rest == b"windows\\sysnative\\bash.exe"
    )
}

fn bash_shell_config(shell: String) -> ShellConfig {
    if is_legacy_wsl_bash_path(&shell, std::env::var("SystemRoot").ok().as_deref()) {
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
            // A relative path names the file found relative to this process, not a program on `PATH`.
            let config = bash_shell_config(custom.to_string());
            let absolute = std::path::absolute(custom).map(|path| path.to_string_lossy().into_owned());
            return Ok(ShellConfig { shell: absolute.unwrap_or(config.shell), ..config });
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

/// `%SystemRoot%`, or `C:\Windows` when it is unset or empty.
#[cfg(any(windows, test))]
fn windows_directory(system_root: Option<std::ffi::OsString>) -> std::path::PathBuf {
    system_root.filter(|root| !root.is_empty()).map(Into::into).unwrap_or_else(|| "C:\\Windows".into())
}

/// Kill a process and its descendants: its process group on Unix, `taskkill /F /T` on Windows. Best effort.
#[cfg(windows)]
pub(crate) fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    let root = windows_directory(std::env::var_os("SystemRoot"));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_wsl_launchers_follow_the_windows_directory() {
        let cases = [
            ("C:\\Windows\\System32\\bash.exe", None, true),
            ("c:/windows/sysnative/BASH.EXE", None, true),
            ("D:\\WinNT\\System32\\bash.exe", Some("D:\\WinNT"), true),
            ("D:\\WinNT\\Sysnative\\bash.exe", Some("D:\\WinNT"), true),
            ("D:/WinNT/./System32/../System32/bash.exe", Some("D:\\WinNT\\"), true),
            ("D:\\WinNT\\System32\\bash.exe", Some(""), false),
            ("D:\\WinNT\\System32\\bash.exe", None, false),
            ("C:\\Windows\\System32\\bash.exe", Some("D:\\WinNT"), false),
            ("E:\\Windows\\System32\\bash.exe", Some("D:\\WinNT"), false),
            ("D:\\WinNT\\System32\\bash.exe", Some("WinNT"), false),
            ("C:\\Git\\bin\\bash.exe", None, false),
        ];
        for (path, root, expected) in cases {
            assert_eq!(is_legacy_wsl_bash_path(path, root), expected, "{path} with SystemRoot={root:?}");
        }
    }

    #[test]
    fn an_empty_system_root_falls_back_to_the_default_windows_directory() {
        let default = std::path::PathBuf::from("C:\\Windows");
        assert_eq!(windows_directory(None), default);
        assert_eq!(windows_directory(Some("".into())), default);
        assert_eq!(windows_directory(Some("D:\\WinNT".into())), std::path::PathBuf::from("D:\\WinNT"));
    }
}
