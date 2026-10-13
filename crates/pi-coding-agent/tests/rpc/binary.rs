//! `pi --mode rpc` as a subprocess with a local mock server (upstream `rpc.test.ts`).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use pi_coding_agent::rpc::jsonl::JsonlReader;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::cli_support::{mock_server, setup, text_body};

pub fn pi_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pi"));
    command
        .args(args)
        .current_dir(root.join("work"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env("HOME", root)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

/// Run to completion with a deadline, so a stuck child fails the test.
async fn finished(mut command: Command) -> std::process::Output {
    let output = tokio::time::timeout(Duration::from_secs(30), command.output()).await;
    output.expect("process exits in time").unwrap()
}

struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: JsonlReader<ChildStdout>,
    lines: Vec<String>,
}

impl Proc {
    fn spawn(root: &Path, args: &[&str]) -> Self {
        let mut command = pi_command(root, &[&["--mode", "rpc"], args].concat());
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = JsonlReader::new(child.stdout.take().unwrap());
        Self { child, stdin, stdout, lines: Vec::new() }
    }

    async fn write(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn send(&mut self, record: Value) {
        self.write(&format!("{record}\n")).await;
    }

    /// Read records until one matches; every stdout line must be JSON.
    async fn until(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        let read = async {
            loop {
                let line = self.stdout.next_line().await.unwrap().expect("stdout open");
                let record: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
                self.lines.push(line);
                if predicate(&record) {
                    return record;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), read).await.expect("record in time")
    }

    async fn response(&mut self, id: &str) -> Value {
        self.until(|record| record["type"] == "response" && record["id"] == id).await
    }

    /// Close stdin; returns the exit code and remaining stdout lines.
    async fn close(mut self) -> (Option<i32>, Vec<String>, String) {
        self.stdin.take();
        let mut rest = Vec::new();
        while let Ok(Some(line)) = self.stdout.next_line().await {
            rest.push(line);
        }
        let output =
            tokio::time::timeout(Duration::from_secs(20), self.child.wait_with_output()).await.unwrap().unwrap();
        (output.status.code(), rest, String::from_utf8_lossy(&output.stderr).to_string())
    }
}

fn session_lines(root: &Path) -> Vec<Value> {
    let sessions = root.join("agent/sessions");
    let dir = std::fs::read_dir(&sessions).unwrap().next().unwrap().unwrap().path();
    let files: Vec<_> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    assert_eq!(files.len(), 1);
    std::fs::read_to_string(&files[0]).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn drives_a_session_and_saves_it() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("hello")]).await;
    setup(root.path(), &url);
    let mut pi = Proc::spawn(root.path(), &["--model", "mock-model"]);
    pi.send(json!({"id": "s", "type": "get_state"})).await;
    let state = pi.response("s").await;
    assert_eq!(
        (state["data"]["model"]["id"].clone(), state["data"]["isStreaming"].clone()),
        (json!("mock-model"), json!(false))
    );
    assert_eq!(state["data"]["messageCount"], 0);

    pi.send(json!({"id": "p", "type": "prompt", "message": "Reply with just the word 'hello'"})).await;
    assert_eq!(pi.response("p").await["data"]["disposition"], "started");
    pi.until(|record| record["type"] == "agent_settled").await;
    let ends = pi.lines.iter().filter(|line| line.contains("\"type\":\"message_end\"")).count();
    assert!(ends >= 2, "user and assistant message_end");
    assert_eq!(requests.lock().unwrap().len(), 1);

    let (code, rest, stderr) = pi.close().await;
    assert_eq!(code, Some(0), "{stderr}");
    assert!(rest.is_empty(), "{rest:?}");
    let entries = session_lines(root.path());
    assert_eq!(entries[0]["type"], "session");
    let roles: Vec<&str> = entries
        .iter()
        .filter(|entry| entry["type"] == "message")
        .map(|entry| entry["message"]["role"].as_str().unwrap())
        .collect();
    assert!(roles.contains(&"user") && roles.contains(&"assistant"), "{roles:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn frames_records_strictly_through_the_binary() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let mut pi = Proc::spawn(root.path(), &["--model", "mock-model", "--no-session"]);
    pi.write("{\"id\":\"a\",\"type\":\"set_session_name\",\"na").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    pi.write("me\":\"x\u{2028}y\"}\r\n").await;
    assert_eq!(pi.response("a").await["success"], true);
    pi.write("garbage\n{\"id\":\"b\",\"type\":\"get_state\"}\n").await;
    let parse = pi.until(|record| record["command"] == "parse").await;
    assert_eq!(parse["success"], false);
    let state = pi.response("b").await;
    assert_eq!(state["data"]["sessionName"], "x\u{2028}y");
    assert!(state["data"].get("sessionFile").is_none(), "--no-session");
    pi.write("{\"id\":\"c\",\"type\":\"get_messages\"}").await;
    let (code, rest, _) = pi.close().await;
    assert_eq!(code, Some(0));
    assert_eq!(rest.len(), 1);
    assert!(rest[0].contains("\"id\":\"c\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn runs_and_aborts_bash_through_the_binary() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let mut pi = Proc::spawn(root.path(), &["--model", "mock-model"]);
    pi.send(json!({"id": "b", "type": "bash", "command": "echo hello"})).await;
    let result = pi.response("b").await;
    assert_eq!(result["data"], json!({"output": "hello\n", "exitCode": 0, "cancelled": false, "truncated": false}));
    pi.send(json!({"id": "long", "type": "bash", "command": "echo go; sleep 30"})).await;
    pi.until(|record| record["type"] == "bash_execution_update" && record["id"] == "long").await;
    pi.send(json!({"id": "ab", "type": "abort_bash"})).await;
    let cancelled = pi.response("long").await;
    assert_eq!(cancelled["data"]["cancelled"], true);
    let (code, _, _) = pi.close().await;
    assert_eq!(code, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_stray_output_off_stdout_and_rejects_file_arguments() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![text_body("elsewhere")]).await;
    setup(root.path(), &url);
    std::fs::create_dir_all(root.path().join("other")).unwrap();
    let mut first = pi_command(root.path(), &["--model", "mock-model", "--session-id", "elsewhere-1", "-p", "hi"]);
    first.current_dir(root.path().join("other"));
    let output = finished(first).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));

    let output = finished(pi_command(root.path(), &["--mode", "rpc", "--session", "elsewhere-1"])).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "stdout carries protocol records only");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Session found in different project") && stderr.contains("Aborted."), "{stderr}");

    std::fs::write(root.path().join("work/notes.txt"), "notes").unwrap();
    let output = finished(pi_command(root.path(), &["--mode", "rpc", "@notes.txt"])).await;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Error: @file arguments are not supported in RPC mode"));
    assert!(output.stdout.is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn exits_with_143_on_sigterm() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let mut pi = Proc::spawn(root.path(), &["--model", "mock-model"]);
    pi.send(json!({"id": "s", "type": "get_state"})).await;
    pi.response("s").await;
    let pid = pi.child.id().unwrap().to_string();
    let status = Command::new("kill").args(["-TERM", &pid]).status().await.unwrap();
    assert!(status.success());
    let status = tokio::time::timeout(Duration::from_secs(20), pi.child.wait()).await.unwrap().unwrap();
    assert_eq!(status.code(), Some(143));
}
