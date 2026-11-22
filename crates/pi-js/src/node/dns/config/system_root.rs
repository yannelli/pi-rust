//! The Windows directory through the kernel's `\SystemRoot` object link. The `SystemRoot`,
//! `windir`, and `PATH` variables come from whoever started pi, so helpers do not use them.

use std::path::PathBuf;

pub(in crate::node::dns) const SYSTEM_ROOT: &str = r"\\?\GLOBALROOT\SystemRoot";

#[cfg(windows)]
pub(in crate::node::dns) fn system32(file: &str) -> PathBuf {
    windows_directory().join("System32").join(file)
}

fn win32_path(path: PathBuf) -> PathBuf {
    if let Some(drive) = path.to_str().and_then(|s| s.strip_prefix(r"\\?\"))
        && drive.as_bytes().get(1..3) == Some(b":\\")
    {
        return PathBuf::from(drive);
    }
    path
}

#[cfg(windows)]
pub(in crate::node::dns) fn windows_directory() -> &'static std::path::Path {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let path = std::fs::canonicalize(SYSTEM_ROOT).unwrap_or_else(|_| PathBuf::from(SYSTEM_ROOT));
        win32_path(path)
    })
}

#[cfg(windows)]
pub(in crate::node::dns) fn command(file: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let windows = windows_directory();
    let directory = windows.join("System32");
    let mut command = std::process::Command::new(system32(file));
    command
        .current_dir(&directory)
        .env("SystemRoot", windows)
        .env("windir", windows)
        .env("PATH", directory)
        .creation_flags(0x0800_0000);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_paths_with_a_drive_letter_use_win32_spelling() {
        assert_eq!(win32_path(PathBuf::from(r"\\?\D:\Windows")), PathBuf::from(r"D:\Windows"));
        assert_eq!(win32_path(PathBuf::from(SYSTEM_ROOT)), PathBuf::from(SYSTEM_ROOT));
    }

    #[cfg(windows)]
    #[test]
    fn the_kernel_root_opens_files_and_starts_system_utilities() {
        let kernel_reg = PathBuf::from(format!(r"{SYSTEM_ROOT}\System32\reg.exe"));
        assert_eq!(std::fs::canonicalize(system32("reg.exe")).unwrap(), std::fs::canonicalize(kernel_reg).unwrap());
        assert!(system32("netsh.exe").is_file() && system32("reg.exe").is_file());
        assert!(system32(r"drivers\etc").is_dir());
        let key = r"HKLM\System\CurrentControlSet\Services\Tcpip\Parameters";
        let mut reg = command("reg.exe");
        reg.args(["query", key]);
        let output = super::super::bounded::try_run(reg, std::time::Duration::from_secs(10), 1 << 20).unwrap();
        assert!(output.success, "reg query exited unsuccessfully");
        let mut netsh = command("netsh.exe");
        netsh.args(["interface", "ipv4", "show", "route"]);
        let bounded = super::super::bounded::MAX_HELPER_OUTPUT;
        let routes = super::super::bounded::try_run(netsh, std::time::Duration::from_secs(10), bounded).unwrap();
        assert!(routes.success);
        assert!(!super::super::windows::parse_routes(&String::from_utf8_lossy(&routes.stdout)).0.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn a_stalled_helper_is_killed_at_the_deadline() {
        use std::time::Duration;
        const PID_FILE: &str = "PI_DNS_TEST_HELPER_PID_FILE";
        if let Some(path) = std::env::var_os(PID_FILE) {
            std::fs::write(path, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "node::dns::config::system_root::tests::a_stalled_helper_is_killed_at_the_deadline"])
            .env(PID_FILE, &pid_file);
        assert!(super::super::bounded::run(child, Duration::from_secs(5), 4096).is_none());
        let pid: u32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let mut tasklist = std::process::Command::new(system32("tasklist.exe"));
        tasklist.args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
        let listed = super::super::bounded::try_run(tasklist, Duration::from_secs(20), 1 << 16).unwrap();
        assert!(listed.success);
        assert!(!String::from_utf8_lossy(&listed.stdout).contains(&format!("\"{pid}\"")));
    }
}
