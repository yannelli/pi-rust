use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi_agent::AgentToolResult;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use super::{run, text};
use crate::tools::create_tool;

#[tokio::test]
async fn runs_in_cwd_and_combines_streams() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("marker.txt"), "").unwrap();
    let result = run("bash", dir.path(), json!({ "command": "ls; echo err >&2; sleep 0.05; echo out" })).await.unwrap();
    assert!(!result.is_error);
    assert_eq!(text(&result), "marker.txt\nerr\nout\n");
    assert!(result.details.is_none());
}

#[tokio::test]
async fn empty_output_placeholder() {
    let dir = tempfile::tempdir().unwrap();
    let result = run("bash", dir.path(), json!({ "command": "true" })).await.unwrap();
    assert_eq!(text(&result), "(no output)");
}

#[tokio::test]
async fn nonzero_exit_is_error_result_with_output() {
    let dir = tempfile::tempdir().unwrap();
    let result = run("bash", dir.path(), json!({ "command": "echo failing; exit 3" })).await.unwrap();
    assert!(result.is_error);
    assert_eq!(text(&result), "failing\n\n\nCommand exited with code 3");
}

#[tokio::test]
async fn timeout_kills_and_reports() {
    let dir = tempfile::tempdir().unwrap();
    let started = Instant::now();
    let err = run("bash", dir.path(), json!({ "command": "echo before; sleep 10", "timeout": 0.5 })).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(err.to_string(), "before\n\n\nCommand timed out after 0.5 seconds");
    let err = run("bash", dir.path(), json!({ "command": "true", "timeout": -1 })).await.unwrap_err();
    assert!(err.to_string().starts_with("Invalid timeout"));
}

fn process_gone(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat.split(") ").nth(1).is_some_and(|rest| rest.starts_with('Z')),
    }
}

#[tokio::test]
async fn cancellation_kills_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_tool("bash", dir.path()).unwrap();
    let cancel = CancellationToken::new();
    let (trigger, pid_file) = (cancel.clone(), dir.path().join("child.pid"));
    tokio::spawn(async move {
        // Cancel once the background child exists, however slowly the shell starts.
        while !std::fs::read_to_string(&pid_file).is_ok_and(|pid| pid.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        trigger.cancel();
    });
    let args = json!({ "command": "sleep 30 & echo $! > child.pid; wait" });
    let started = Instant::now();
    let err = tool.execute("c", args, cancel, super::noop()).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(err.to_string(), "Command aborted");

    let pid = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
    let pid = pid.trim().to_string();
    // SIGKILL is asynchronous: a loaded CI runner can take seconds to schedule the exit.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !process_gone(&pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    assert!(process_gone(&pid), "background child {pid} still running: {stat}");
}

#[tokio::test]
async fn truncates_tail_and_saves_full_output() {
    let dir = tempfile::tempdir().unwrap();
    let result = run("bash", dir.path(), json!({ "command": "seq 1 3000" })).await.unwrap();
    let out = text(&result);
    assert!(out.starts_with("1001\n"), "{}", &out[..20]);
    let details = result.details.unwrap();
    let path = details["fullOutputPath"].as_str().unwrap().to_string();
    assert!(out.ends_with(&format!("[Showing lines 1001-3000 of 3000. Full output: {path}]")), "{out}");
    assert_eq!(details["truncation"]["truncatedBy"], "lines");
    let full = std::fs::read_to_string(&path).unwrap();
    assert_eq!(full.lines().count(), 3000);
    assert!(full.starts_with("1\n2\n"));
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn byte_truncation_mentions_limit() {
    let dir = tempfile::tempdir().unwrap();
    let cmd = "for i in $(seq 1 1000); do printf '%0100d\\n' $i; done";
    let result = run("bash", dir.path(), json!({ "command": cmd })).await.unwrap();
    let out = text(&result);
    assert!(out.contains("of 1000 (50.0KB limit). Full output: "), "{}", &out[out.len() - 150..]);
    let path = result.details.unwrap()["fullOutputPath"].as_str().unwrap().to_string();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 101 * 1000);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn streams_partial_updates() {
    let dir = tempfile::tempdir().unwrap();
    let tool = create_tool("bash", dir.path()).unwrap();
    let updates: Arc<Mutex<Vec<AgentToolResult>>> = Arc::default();
    let sink = updates.clone();
    let on_update = Arc::new(move |r: AgentToolResult| sink.lock().unwrap().push(r));
    let args = json!({ "command": "echo first; sleep 0.3; echo second" });
    let result = tool.execute("c", args, CancellationToken::new(), on_update).await.unwrap();
    assert_eq!(text(&result), "first\nsecond\n");
    let updates = updates.lock().unwrap();
    assert!(updates[0].content.is_empty());
    assert!(updates.iter().any(|u| text(u) == "first\n"));
    assert_eq!(text(updates.last().unwrap()), "first\nsecond\n");
}

#[cfg(unix)]
#[tokio::test]
async fn custom_shell_path_and_command_prefix() {
    use pi_agent::AgentTool;

    use crate::tools::BashTool;
    let dir = tempfile::tempdir().unwrap();
    let call = |tool: BashTool, command: &str| {
        let args = json!({ "command": command });
        async move { tool.execute("call-1", args, CancellationToken::new(), super::noop()).await }
    };
    let tool = BashTool::new(dir.path().to_path_buf())
        .with_shell_path(Some("/bin/sh".into()))
        .with_command_prefix(Some("PREFIXED=yes".into()));
    let result = call(tool, "echo $PREFIXED").await.unwrap();
    assert_eq!(text(&result), "yes\n");

    let missing = BashTool::new(dir.path().to_path_buf()).with_shell_path(Some(dir.path().join("no-shell")));
    let error = call(missing, "true").await.unwrap_err().to_string();
    assert!(error.starts_with("Custom shell path not found: "), "{error}");
}
