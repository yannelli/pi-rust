//! The hosts file c-ares `ares_hosts_path` picks for `ares_gethostbyaddr`; `CARES_HOSTS` applies
//! only to address lookups. Windows reads `DatabasePath` with `reg.exe`, once per channel.

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

#[cfg(windows)]
pub(super) fn path() -> Option<PathBuf> {
    hosts_file(&expand(&database_directory(run_reg)?, |name| std::env::var(name).ok()))
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
    (file.len() < MAX_PATH).then(|| PathBuf::from(file))
}

/// `ExpandEnvironmentStrings`: `%NAME%` becomes the variable's value, and an unknown name
/// stays as written.
#[cfg(any(windows, test))]
fn expand(text: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match (!name.is_empty()).then(|| lookup(name)).flatten() {
            Some(value) => {
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                out.push('%');
                out.push_str(name);
                rest = &after[end..];
            }
        }
    }
    out.push_str(rest);
    out
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
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let mut command = Command::new(std::path::Path::new(&root).join("System32\\reg.exe"));
    command.args(["query", KEY]);
    if value {
        command.args(["/v", "DatabasePath"]);
    }
    let output = command.stdin(Stdio::null()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    Some((output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment(name: &str) -> Option<String> {
        (name == "SystemRoot").then(|| "C:\\Windows".to_string())
    }

    #[test]
    fn reads_and_expands_the_registry_database_path() {
        let output = "\r\nHKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Services\\Tcpip\\Parameters\r\n    \
            DatabasePath    REG_EXPAND_SZ    %SystemRoot%\\My Hosts\r\n\r\n";
        let value = registry_value(output).unwrap();
        assert_eq!(value, "%SystemRoot%\\My Hosts");
        assert_eq!(hosts_file(&expand(&value, environment)), Some(PathBuf::from("C:\\Windows\\My Hosts\\hosts")));
        assert_eq!(registry_value("ERROR: The system was unable to find the specified registry key or value."), None);
        let default = expand(DEFAULT_DATABASE, environment);
        assert_eq!(default, "C:\\Windows\\System32\\drivers\\etc");
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
    fn unknown_names_stay_and_long_paths_are_refused() {
        assert_eq!(expand("%Missing%\\%SystemRoot%\\50%", environment), "%Missing%\\C:\\Windows\\50%");
        assert_eq!(expand("%%SystemRoot%", environment), "%C:\\Windows");
        assert_eq!(hosts_file(&"a".repeat(253)), Some(PathBuf::from(format!("{}\\hosts", "a".repeat(253)))));
        assert_eq!(hosts_file(&"a".repeat(254)), None);
    }
}
