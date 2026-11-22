//! The hosts file c-ares `ares_hosts_path` picks for `ares_gethostbyaddr`; `CARES_HOSTS` applies
//! only to address lookups. Windows reads `DatabasePath` with `reg.exe` on each call and
//! expands only `%SystemRoot%` and `%windir%`, to the kernel's `\SystemRoot` link.

use std::path::PathBuf;

#[cfg(not(windows))]
pub(super) fn path() -> Option<PathBuf> {
    Some(PathBuf::from("/etc/hosts"))
}

/// The value Windows installs, used when the key is readable but the value is not, when
/// `reg.exe` cannot run, or when its output has no `DatabasePath` row.
#[cfg(any(windows, test))]
const DEFAULT_DATABASE: &str = "%SystemRoot%\\System32\\drivers\\etc";
#[cfg(any(windows, test))]
const MAX_PATH: usize = 260;
#[cfg(windows)]
const KEY: &str = "HKLM\\System\\CurrentControlSet\\Services\\Tcpip\\Parameters";

#[cfg(any(windows, test))]
use super::config::system_root::SYSTEM_ROOT;

#[cfg(windows)]
pub(super) fn path() -> Option<PathBuf> {
    hosts_file(&trusted_directory(&database_directory(run_reg)?)?)
}

/// Query the registry key itself only after its value lookup fails. An unreadable key gives
/// no hosts file, as in c-ares. An unavailable system utility uses the Windows default.
#[cfg(any(windows, test))]
fn database_directory(run: impl Fn(bool) -> Option<(bool, String)>) -> Option<String> {
    match run(true) {
        Some((true, output)) => registry_value(&output),
        Some((false, _)) if matches!(run(false), Some((false, _))) => return None,
        _ => None,
    }
    .or_else(|| Some(DEFAULT_DATABASE.to_string()))
}

/// c-ares appends `\hosts` and gives up when the result does not fit in `MAX_PATH`.
#[cfg(any(windows, test))]
fn hosts_file(directory: &str) -> Option<PathBuf> {
    let file = format!("{directory}\\hosts");
    #[cfg(windows)]
    let length = file
        .strip_prefix(SYSTEM_ROOT)
        .map_or(file.len(), |rest| super::config::system_root::windows_directory().as_os_str().len() + rest.len());
    #[cfg(not(windows))]
    let length = file.len();
    (length < MAX_PATH).then(|| PathBuf::from(file))
}

/// Accepts absolute registry paths and expands a leading `%SystemRoot%` or `%windir%` from the kernel.
/// Other variables and relative paths depend on the process that launched pi.
#[cfg(any(windows, test))]
fn trusted_directory(value: &str) -> Option<String> {
    let variable = |text: &str| text.find('%').is_some_and(|at| text[at + 1..].contains('%'));
    let rooted = ["%SystemRoot%", "%windir%"].iter().find_map(|name| {
        let head = value.get(..name.len())?;
        head.eq_ignore_ascii_case(name).then(|| &value[name.len()..])
    });
    let Some(rest) = rooted else {
        let bytes = value.as_bytes();
        let drive = bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let absolute =
            (drive && matches!(bytes[2], b'\\' | b'/')) || value.starts_with(r"\\") || value.starts_with("//");
        return (absolute && !variable(value)).then(|| value.to_string());
    };
    if variable(rest) || !(rest.is_empty() || rest.starts_with(['\\', '/'])) {
        return None;
    }
    // A `\\?\` path skips Win32 normalization, so `.`, `..`, and `/` are resolved here.
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    Some(std::iter::once(SYSTEM_ROOT).chain(parts).collect::<Vec<_>>().join("\\"))
}

/// The data of the `DatabasePath` row in `reg query` output.
#[cfg(any(windows, test))]
fn registry_value(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("DatabasePath")?;
        let (kind, value) = rest.trim_start().split_once(char::is_whitespace)?;
        let value = value.trim();
        (kind.starts_with("REG_") && !value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(windows)]
fn run_reg(value: bool) -> Option<(bool, String)> {
    use super::config::{bounded, system_root};
    const MAX_OUTPUT: usize = 64 << 10;
    let mut command = system_root::command("reg.exe");
    command.args(["query", KEY]);
    if value {
        command.args(["/v", "DatabasePath"]);
    }
    let output = bounded::run(command, bounded::HELPER_DEADLINE, MAX_OUTPUT)?;
    Some((output.success, String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_registry_database_path_onto_the_kernel_root() {
        let output = "\r\nHKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\Tcpip\\Parameters\r\n    \
            DatabasePath    REG_EXPAND_SZ    %SystemRoot%\\My Hosts\r\n\r\n";
        let value = registry_value(output).unwrap();
        assert_eq!(value, "%SystemRoot%\\My Hosts");
        let file = hosts_file(&trusted_directory(&value).unwrap());
        assert_eq!(file, Some(PathBuf::from(r"\\?\GLOBALROOT\SystemRoot\My Hosts\hosts")));
        assert_eq!(registry_value("ERROR: The system was unable to find the specified registry key or value."), None);
        let default = trusted_directory(DEFAULT_DATABASE).unwrap();
        assert_eq!(default, r"\\?\GLOBALROOT\SystemRoot\System32\drivers\etc");
    }

    #[test]
    fn keeps_absolute_paths_and_refuses_environment_and_relative_paths() {
        for (value, expected) in [
            (r"D:\etc", Some(r"D:\etc")),
            ("d:/etc", Some("d:/etc")),
            (r"\\server\share\etc", Some(r"\\server\share\etc")),
            (r"D:\50% off", Some(r"D:\50% off")),
            (r"%WINDIR%/System32/./x/../drivers//etc\", Some(r"\\?\GLOBALROOT\SystemRoot\System32\drivers\etc")),
            ("%SystemRoot%", Some(SYSTEM_ROOT)),
            (r"%SystemRoot%\..\..\etc", Some(r"\\?\GLOBALROOT\SystemRoot\etc")),
            (r"%SystemDrive%\etc", None),
            (r"%SystemRoot%\%USERPROFILE%", None),
            (r"D:\%TEMP%\etc", None),
            (r"%SystemRoot%etc", None),
            (r"etc", None),
            (r"\etc", None),
            (r"D:etc", None),
            ("", None),
        ] {
            assert_eq!(trusted_directory(value).as_deref(), expected, "{value}");
        }
    }

    #[test]
    fn an_unreadable_key_gives_no_file_and_other_failures_use_the_default() {
        let row = "    DatabasePath    REG_EXPAND_SZ    D:\\etc\r\n".to_string();
        let calls = std::cell::RefCell::new(Vec::new());
        let calls = &calls;
        let reg = |answers: [Option<(bool, String)>; 2]| {
            move |value: bool| {
                calls.borrow_mut().push(value);
                answers[usize::from(!value)].clone()
            }
        };
        let default = Some(DEFAULT_DATABASE.to_string());
        assert_eq!(database_directory(reg([Some((true, row)), None])), Some("D:\\etc".into()));
        assert_eq!(calls.take(), [true]);
        assert_eq!(database_directory(reg([Some((false, String::new())), Some((false, String::new()))])), None);
        assert_eq!(database_directory(reg([Some((false, String::new())), Some((true, String::new()))])), default);
        assert_eq!(database_directory(reg([Some((false, String::new())), None])), default);
        assert_eq!(calls.take(), [true, false, true, false, true, false]);
        assert_eq!(database_directory(reg([None, None])), default);
        assert_eq!(database_directory(reg([Some((true, "garbage".into())), None])), default);
        assert_eq!(calls.take(), [true, true]);
    }

    #[test]
    fn long_paths_are_refused() {
        assert_eq!(hosts_file(&"a".repeat(253)), Some(PathBuf::from(format!("{}\\hosts", "a".repeat(253)))));
        assert_eq!(hosts_file(&"a".repeat(254)), None);
    }
}
