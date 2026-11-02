//! Windows parts of `project_trust_store_exports`: paths without the verbatim prefix, child process cleanup, and the
//! Windows-only path cases against a golden from upstream on Node on Windows.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `CREATE_NEW_PROCESS_GROUP` from the Windows process creation flags.
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// How long `taskkill` itself may run before it is killed and reaped.
const TASKKILL_LIMIT: Duration = Duration::from_secs(10);

const WINDOWS_SCENARIO: (&str, &str) = (
    include_str!("../fixtures/extensions/project_trust_store_windows_scenario.json"),
    "project_trust_store_windows_scenario.json",
);
const WINDOWS_MODULE: &str = include_str!("../fixtures/extensions/project_trust_store_windows_scenario.mjs");

/// `canonicalize` without the `\\?\` prefix it adds to drive paths, so paths have the form Node's `realpathSync` gives.
pub fn canonical(path: &Path) -> PathBuf {
    let path = std::fs::canonicalize(path).unwrap();
    let text = path.to_str().unwrap_or_default();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

pub fn own_group(command: &mut Command) {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

/// Ends the child's process tree with `taskkill /T /F` while this open handle keeps the child's id from being
/// reused, then kills and reaps the child itself. The `taskkill` result is printed as evidence.
pub fn stop(child: &mut Child, limit: Duration) {
    let id = child.id();
    let tree = match std::env::var_os("SystemRoot") {
        Some(root) => bounded(
            Command::new(PathBuf::from(root).join("System32").join("taskkill.exe"))
                .args(["/PID", &id.to_string(), "/T", "/F"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        ),
        None => "not run: SystemRoot is unset".to_string(),
    };
    let killed = child.kill();
    let _ = child.wait();
    println!("taskkill /T /F of {id} after {limit:?}: {tree}; kill: {killed:?}");
}

/// Runs `command` for at most [`TASKKILL_LIMIT`], then kills and reaps it; returns its status or timeout as evidence.
fn bounded(command: &mut Command) -> String {
    let mut tool = match command.spawn() {
        Ok(tool) => tool,
        Err(error) => return format!("not started: {error}"),
    };
    let started = Instant::now();
    loop {
        match tool.try_wait() {
            Ok(Some(status)) => return format!("exited with {status}"),
            Ok(None) if started.elapsed() <= TASKKILL_LIMIT => std::thread::sleep(Duration::from_millis(50)),
            waited => {
                let killed = tool.kill();
                let _ = tool.wait();
                return format!("stopped after {:?} ({waited:?}); kill: {killed:?}", started.elapsed());
            }
        }
    }
}

/// Case folding, drive and shell path forms, ancestor bounds, junctions and directory symlinks, as upstream does
/// them on Windows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_path_cases_match_upstream() {
    let expected = super::golden(WINDOWS_SCENARIO);
    let actual = super::run_module("project_trust_store_windows_scenario.mjs", WINDOWS_MODULE).await;
    assert_eq!(actual.keys().collect::<Vec<_>>(), expected.keys().collect::<Vec<_>>());
    for (name, case) in &expected {
        assert_eq!(&actual[name], case, "{name}");
    }
    println!("project trust store Windows cases: {} compared", expected.len());
}
