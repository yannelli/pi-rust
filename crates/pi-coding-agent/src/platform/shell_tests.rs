use std::collections::{HashMap, HashSet};

use super::*;

#[derive(Default)]
struct FakeLookup {
    files: HashSet<String>,
    vars: HashMap<String, String>,
}

impl FakeLookup {
    fn new(files: &[&str], vars: &[(&str, &str)]) -> Self {
        Self {
            files: files.iter().map(|file| file.to_string()).collect(),
            vars: vars.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect(),
        }
    }
}

impl ShellLookup for FakeLookup {
    fn exists(&self, path: &str) -> bool {
        self.files.contains(path)
    }

    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }
}

fn windows(lookup: &FakeLookup, custom: Option<&str>) -> Result<ShellConfig, String> {
    resolve_shell_config(custom, ShellPlatform::Windows, lookup)
}

#[test]
fn windows_prefers_git_bash_in_program_files() {
    let lookup = FakeLookup::new(
        &[r"C:\Program Files (x86)\Git\bin\bash.exe", r"C:\msys64\usr\bin\bash.exe"],
        &[
            ("ProgramFiles", r"C:\Program Files"),
            ("ProgramFiles(x86)", r"C:\Program Files (x86)"),
            ("PATH", r"C:\msys64\usr\bin"),
        ],
    );
    let config = windows(&lookup, None).unwrap();
    assert_eq!(config.shell, r"C:\Program Files (x86)\Git\bin\bash.exe");
    assert_eq!(config.args, ["-c"]);
    assert_eq!(config.command_transport, CommandTransport::Argv);
}

#[test]
fn windows_falls_back_to_bash_on_path() {
    let lookup = FakeLookup::new(
        &[r"C:\cygwin64\bin\bash.exe"],
        &[("ProgramFiles", r"C:\Program Files"), ("Path", r"C:\Tools;C:\cygwin64\bin\")],
    );
    assert_eq!(windows(&lookup, None).unwrap().shell, r"C:\cygwin64\bin\bash.exe");
}

#[test]
fn legacy_wsl_bash_reads_commands_from_stdin() {
    let lookup = FakeLookup::new(&[r"C:\Windows\System32\bash.exe"], &[("PATH", r"C:\Windows\System32;C:\Windows")]);
    let config = windows(&lookup, None).unwrap();
    assert_eq!(config.args, ["-s"]);
    assert_eq!(config.command_transport, CommandTransport::Stdin);
    assert!(is_legacy_wsl_bash_path("c:/windows/sysnative/BASH.EXE"));
    assert!(!is_legacy_wsl_bash_path(r"C:\Program Files\Git\bin\bash.exe"));
}

#[test]
fn windows_reports_searched_locations_when_bash_is_missing() {
    let lookup = FakeLookup::new(&[], &[("ProgramFiles", r"C:\Program Files"), ("PATH", r"C:\Windows")]);
    let error = windows(&lookup, None).unwrap_err();
    assert!(error.starts_with("No bash shell found. Options:\n  1. Install Git for Windows"), "{error}");
    assert!(error.ends_with("Searched Git Bash in:\n  C:\\Program Files\\Git\\bin\\bash.exe"), "{error}");
}

#[test]
fn custom_shell_path_wins_or_fails() {
    let lookup = FakeLookup::new(&[r"D:\bash.exe", "/bin/bash"], &[]);
    assert_eq!(windows(&lookup, Some(r"D:\bash.exe")).unwrap().shell, r"D:\bash.exe");
    let error = resolve_shell_config(Some("/opt/zsh"), ShellPlatform::Unix, &lookup).unwrap_err();
    assert_eq!(error, "Custom shell path not found: /opt/zsh");
}

#[test]
fn unix_uses_bin_bash_then_path_then_sh() {
    let unix = |lookup: &FakeLookup| resolve_shell_config(None, ShellPlatform::Unix, lookup).unwrap().shell;
    assert_eq!(unix(&FakeLookup::new(&["/bin/bash"], &[])), "/bin/bash");
    let termux = FakeLookup::new(
        &["/data/data/com.termux/files/usr/bin/bash"],
        &[("PATH", "/data/data/com.termux/files/usr/bin:/system/bin")],
    );
    assert_eq!(unix(&termux), "/data/data/com.termux/files/usr/bin/bash");
    assert_eq!(unix(&FakeLookup::new(&[], &[("PATH", "/usr/bin")])), "sh");
}

#[test]
fn powershell_prefers_pwsh_on_windows_only() {
    let both =
        FakeLookup::new(&[r"C:\pwsh\pwsh.exe", r"C:\Windows\powershell.exe"], &[("PATH", r"C:\Windows;C:\pwsh")]);
    let config = resolve_powershell_config(ShellPlatform::Windows, &both).unwrap();
    assert_eq!(config.shell, r"C:\pwsh\pwsh.exe");
    assert_eq!(config.args, POWERSHELL_ARGS);
    let legacy = FakeLookup::new(&[r"C:\Windows\powershell.exe"], &[("PATH", r"C:\Windows")]);
    assert_eq!(resolve_powershell_config(ShellPlatform::Windows, &legacy).unwrap().shell, r"C:\Windows\powershell.exe");
    let none = FakeLookup::new(&[], &[("PATH", r"C:\Windows")]);
    assert!(resolve_powershell_config(ShellPlatform::Windows, &none).unwrap_err().starts_with("No PowerShell"));
    let error = resolve_powershell_config(ShellPlatform::Unix, &both).unwrap_err();
    assert_eq!(error, "The powershell tool is only available on Windows.");
}

#[test]
fn bin_dir_is_prepended_once() {
    assert_eq!(path_with_bin_dir("/usr/bin", "/home/u/.pi/agent/bin", ':'), "/home/u/.pi/agent/bin:/usr/bin");
    assert_eq!(path_with_bin_dir("/a:/usr/bin", "/a", ':'), "/a:/usr/bin");
    assert_eq!(path_with_bin_dir("", "/a", ':'), "/a");
    assert_eq!(path_with_bin_dir(r"C:\x", r"C:\bin", ';'), r"C:\bin;C:\x");
}

#[test]
fn binary_output_keeps_whitespace_controls() {
    assert_eq!(sanitize_binary_output("a\u{0}b\tc\r\n\u{1b}d\u{fffa}e😀"), "ab\tc\r\nde😀");
}
