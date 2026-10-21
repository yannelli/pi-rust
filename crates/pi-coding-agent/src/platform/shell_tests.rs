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
    assert!(is_legacy_wsl_bash_path("c:/windows/sysnative/BASH.EXE", None));
    assert!(!is_legacy_wsl_bash_path(r"C:\Program Files\Git\bin\bash.exe", None));
}

#[test]
fn legacy_wsl_bash_is_found_under_the_windows_directory() {
    let cases = [
        (Some(r"D:\WinNT\"), "d:/WINNT/System32/BASH.EXE", true),
        (Some(r"D:\WinNT"), r"D:\WinNT\Sysnative\bash.exe", true),
        (Some(r"D:\WinNT"), r"D:\WinNT\.\Tools\..\System32\\bash.exe", true),
        (None, r"E:\Windows\System32\bash.exe", true),
        (Some(""), r"E:\Windows\System32\bash.exe", true),
        (Some(r"D:\WinNT"), r"D:\Git\bin\bash.exe", false),
        (Some(r"D:\WinNT"), r"E:\Windows\System32\bash.exe", false),
        (Some(r"D:\WinNT"), r"D:\WinNT\System32\bash.exe\", false),
        (Some("WinNT"), r"C:\Windows\System32\bash.exe", false),
        (Some(""), r"D:\WinNT\System32\bash.exe", false),
    ];
    for (system_root, shell, expected) in cases {
        assert_eq!(is_legacy_wsl_bash_path(shell, system_root), expected, "{system_root:?} {shell}");
    }
}

#[test]
fn shell_config_reads_the_windows_directory_from_the_environment() {
    let files = [r"D:\WinNT\System32\bash.exe"];
    let custom = |vars: &[(&str, &str)]| windows(&FakeLookup::new(&files, vars), Some(files[0])).unwrap();
    let config = custom(&[("SystemRoot", r"D:\WinNT")]);
    assert_eq!((config.args, config.command_transport), (vec!["-s".to_string()], CommandTransport::Stdin));
    assert_eq!(custom(&[]).command_transport, CommandTransport::Argv);
    assert_eq!(custom(&[("SystemRoot", r"E:\Windows")]).command_transport, CommandTransport::Argv);
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
fn powershell_discovery_checks_installation_directories() {
    let pwsh7 = r"D:\Applications\PowerShell\7\pwsh.exe";
    let system = r"E:\WinNT\System32\WindowsPowerShell\v1.0\powershell.exe";
    let vars = |path: Option<&'static str>| {
        let mut vars = vec![("ProgramFiles", r"D:\Applications"), ("SystemRoot", r"E:\WinNT")];
        vars.extend(path.map(|path| ("PATH", path)));
        vars
    };
    let find = |files: &[&str], path: Option<&'static str>| {
        let lookup = FakeLookup::new(files, &vars(path));
        resolve_powershell_config(ShellPlatform::Windows, &lookup).map(|config| config.shell)
    };

    let all = [r"E:\Tools\pwsh.exe", r"E:\Tools\powershell.exe", pwsh7, system];
    assert_eq!(find(&all, Some(r"E:\Tools")).unwrap(), r"E:\Tools\pwsh.exe");
    assert_eq!(find(&all[1..], Some(r"E:\Tools")).unwrap(), pwsh7);
    assert_eq!(find(&[r"E:\Tools\powershell.exe", system], Some(r"E:\Tools")).unwrap(), r"E:\Tools\powershell.exe");
    assert_eq!(find(&[system], None).unwrap(), system);
    assert_eq!(
        find(&[], None).unwrap_err(),
        "No PowerShell executable found. Install PowerShell or add powershell.exe/pwsh.exe to PATH."
    );
    let unset = FakeLookup::new(&[pwsh7, system], &[]);
    assert!(resolve_powershell_config(ShellPlatform::Windows, &unset).is_err());
}

#[test]
fn taskkill_falls_back_when_system_root_is_unset_or_empty() {
    let expected = |root: &str| Path::new(root).join("System32").join("taskkill.exe");
    assert_eq!(taskkill_exe(Some(r"D:\CustomWindows".into())), expected(r"D:\CustomWindows"));
    assert_eq!(taskkill_exe(None), expected(r"C:\Windows"));
    assert_eq!(taskkill_exe(Some("".into())), expected(r"C:\Windows"));
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
