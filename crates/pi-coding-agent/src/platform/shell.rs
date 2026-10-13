//! Shell resolution for the bash tool and `!` commands, PowerShell for the Windows
//! `powershell` tool, and the shell `PATH` (upstream `utils/shell.ts`, `docs/windows.md`).

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandTransport {
    /// The command is the argument after `args`.
    Argv,
    /// The command is written to the shell's stdin (legacy WSL `bash.exe -s`).
    Stdin,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellConfig {
    pub shell: String,
    pub args: Vec<String>,
    pub command_transport: CommandTransport,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellPlatform {
    Windows,
    Unix,
}

impl ShellPlatform {
    pub fn current() -> Self {
        if cfg!(windows) { Self::Windows } else { Self::Unix }
    }
}

/// Filesystem and environment lookups, injectable for tests.
pub trait ShellLookup {
    fn exists(&self, path: &str) -> bool;
    fn var(&self, name: &str) -> Option<String>;

    /// First `PATH` entry that contains `executable`.
    fn find_on_path(&self, executable: &str, platform: ShellPlatform) -> Option<String> {
        let path = self.var("PATH").or_else(|| self.var("Path"))?;
        find_executable_on_path(executable, &path, platform, |candidate| self.exists(candidate))
    }
}

/// The real filesystem and process environment.
pub struct SystemShellLookup;

impl ShellLookup for SystemShellLookup {
    fn exists(&self, path: &str) -> bool {
        Path::new(path).is_file()
    }

    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

/// Search `path_var` (split by `;` on Windows and `:` elsewhere) for `executable`.
pub fn find_executable_on_path(
    executable: &str,
    path_var: &str,
    platform: ShellPlatform,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let (delimiter, separator) = match platform {
        ShellPlatform::Windows => (';', '\\'),
        ShellPlatform::Unix => (':', '/'),
    };
    path_var.split(delimiter).map(|dir| dir.trim().trim_matches('"')).filter(|dir| !dir.is_empty()).find_map(|dir| {
        let candidate = format!("{}{separator}{executable}", dir.trim_end_matches(['/', '\\']));
        exists(&candidate).then_some(candidate)
    })
}

/// `C:\Windows\System32\bash.exe` and `Sysnative`, the WSL launcher that reads commands from stdin.
pub fn is_legacy_wsl_bash_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    let bytes = normalized.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_lowercase() || &bytes[1..3] != b":\\" {
        return false;
    }
    matches!(&normalized[3..], "windows\\system32\\bash.exe" | "windows\\sysnative\\bash.exe")
}

fn bash_shell_config(shell: &str) -> ShellConfig {
    if is_legacy_wsl_bash_path(shell) {
        ShellConfig { shell: shell.into(), args: vec!["-s".into()], command_transport: CommandTransport::Stdin }
    } else {
        ShellConfig { shell: shell.into(), args: vec!["-c".into()], command_transport: CommandTransport::Argv }
    }
}

/// `shellPath` when given, then Git Bash and `bash.exe` on `PATH` on Windows, or `/bin/bash`,
/// `bash` on `PATH`, and `sh` elsewhere.
pub fn resolve_shell_config(
    custom_shell_path: Option<&str>,
    platform: ShellPlatform,
    lookup: &dyn ShellLookup,
) -> Result<ShellConfig, String> {
    if let Some(custom) = custom_shell_path.filter(|path| !path.is_empty()) {
        if lookup.exists(custom) {
            return Ok(bash_shell_config(custom));
        }
        return Err(format!("Custom shell path not found: {custom}"));
    }
    if platform == ShellPlatform::Windows {
        let mut paths = Vec::new();
        if let Some(program_files) = lookup.var("ProgramFiles") {
            paths.push(format!("{program_files}\\Git\\bin\\bash.exe"));
        }
        if let Some(program_files_x86) = lookup.var("ProgramFiles(x86)") {
            paths.push(format!("{program_files_x86}\\Git\\bin\\bash.exe"));
        }
        if let Some(path) = paths.iter().find(|path| lookup.exists(path)) {
            return Ok(bash_shell_config(path));
        }
        if let Some(bash) = lookup.find_on_path("bash.exe", platform) {
            return Ok(bash_shell_config(&bash));
        }
        let searched: Vec<String> = paths.iter().map(|path| format!("  {path}")).collect();
        return Err(format!(
            "No bash shell found. Options:\n  1. Install Git for Windows: https://git-scm.com/download/win\n  \
             2. Add your bash to PATH (Cygwin, MSYS2, etc.)\n  3. Set shellPath in settings.json\n\n\
             Searched Git Bash in:\n{}",
            searched.join("\n")
        ));
    }
    if lookup.exists("/bin/bash") {
        return Ok(bash_shell_config("/bin/bash"));
    }
    if let Some(bash) = lookup.find_on_path("bash", platform) {
        return Ok(bash_shell_config(&bash));
    }
    Ok(ShellConfig { shell: "sh".into(), args: vec!["-c".into()], command_transport: CommandTransport::Argv })
}

/// [`resolve_shell_config`] for this process.
pub fn shell_config(custom_shell_path: Option<&Path>) -> Result<ShellConfig, String> {
    let custom = custom_shell_path.map(|path| path.to_string_lossy().into_owned());
    resolve_shell_config(custom.as_deref(), ShellPlatform::current(), &SystemShellLookup)
}

pub const POWERSHELL_ARGS: [&str; 5] = ["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"];

/// `pwsh.exe`, then `powershell.exe`, on Windows only.
pub fn resolve_powershell_config(platform: ShellPlatform, lookup: &dyn ShellLookup) -> Result<ShellConfig, String> {
    if platform != ShellPlatform::Windows {
        return Err("The powershell tool is only available on Windows.".into());
    }
    let shell = lookup
        .find_on_path("pwsh.exe", platform)
        .or_else(|| lookup.find_on_path("powershell.exe", platform))
        .ok_or("No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH.")?;
    let args = POWERSHELL_ARGS.iter().map(|arg| arg.to_string()).collect();
    Ok(ShellConfig { shell, args, command_transport: CommandTransport::Argv })
}

/// [`resolve_powershell_config`] for this process.
pub fn powershell_config() -> Result<ShellConfig, String> {
    resolve_powershell_config(ShellPlatform::current(), &SystemShellLookup)
}

/// `current` with `bin_dir` prepended unless it is already an entry.
pub fn path_with_bin_dir(current: &str, bin_dir: &str, delimiter: char) -> String {
    if current.split(delimiter).any(|entry| entry == bin_dir) {
        return current.to_string();
    }
    if current.is_empty() { bin_dir.to_string() } else { format!("{bin_dir}{delimiter}{current}") }
}

/// The `PATH` variable name (Windows keeps its own casing, such as `Path`) and its value with
/// `bin_dir`, where pi installs helper tools, prepended.
pub fn shell_path_env(bin_dir: &Path) -> (String, String) {
    let key = std::env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .find(|key| key.eq_ignore_ascii_case("path"))
        .unwrap_or_else(|| "PATH".into());
    let current = std::env::var(&key).unwrap_or_default();
    let delimiter = if cfg!(windows) { ';' } else { ':' };
    let value = path_with_bin_dir(&current, &bin_dir.to_string_lossy(), delimiter);
    (key, value)
}

/// Drop control characters other than tab, newline, and carriage return, and U+FFF9 to U+FFFB.
pub fn sanitize_binary_output(text: &str) -> String {
    text.chars()
        .filter(|&ch| {
            let code = ch as u32;
            let control = code <= 0x1f && !matches!(ch, '\t' | '\n' | '\r');
            !control && !(0xfff9..=0xfffb).contains(&code)
        })
        .collect()
}

#[cfg(test)]
#[path = "shell_tests.rs"]
mod tests;
