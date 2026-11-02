#![cfg(unix)]

use std::process::{Command, Stdio};

#[test]
fn reports_deleted_cwd_for_version_help_and_normal_startup() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("deleted");
    for args in [&["--version"][..], &["--help"][..], &[][..]] {
        std::fs::create_dir(&cwd).unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", "/bin/rmdir \"$1\" && shift && exec \"$@\"", "deleted-cwd"])
            .arg(&cwd)
            .arg(env!("CARGO_BIN_EXE_pi"))
            .args(["--offline", "--no-extensions"])
            .args(args)
            .current_dir(&cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path().join("home"))
            .env("PI_CODING_AGENT_DIR", root.path().join("agent"))
            .env("PI_OFFLINE", "1")
            .env("PI_SKIP_VERSION_CHECK", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "Error: The current working directory no longer exists. Change to an existing directory and try again.\n",
            "{args:?}"
        );
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}
