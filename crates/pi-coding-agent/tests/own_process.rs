//! Tests that change process-wide state (the ACP host, keybindings, the panic hook, the offline
//! mark) need a process of their own. nextest gives each test one; `cargo test` runs every test of
//! this binary in one process, so there such a test runs again alone in a child process.

use std::process::Command;

const CHILD_ENV: &str = "PI_TEST_OWN_PROCESS";

/// True when the test `path` (`module_path!()::name`) can run here: under nextest, or in the
/// child. Otherwise runs it alone in a child process of this binary, asserts that it passed, and
/// returns false.
pub fn run_alone(path: &str) -> bool {
    let nextest = std::env::var("NEXTEST_EXECUTION_MODE").is_ok_and(|mode| mode == "process-per-test");
    if nextest || std::env::var_os(CHILD_ENV).is_some() {
        return true;
    }
    let name = path.split_once("::").unwrap().1;
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success() && stdout.contains(" 1 passed"), "{name} in its own process:\n{stdout}\n{stderr}");
    false
}

/// Returns from the calling test `$test` after running it in a child process, unless it already
/// runs alone in its process (see [`run_alone`]).
macro_rules! alone {
    ($test:ident) => {
        if !$crate::own_process::run_alone(concat!(module_path!(), "::", stringify!($test))) {
            return;
        }
    };
}
pub(crate) use alone;
