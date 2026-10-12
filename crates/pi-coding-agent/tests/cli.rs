//! End-to-end runs of the `pi` binary against a local OpenAI-compatible server.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves scripted SSE bodies in order and records each request body.
async fn mock_server(bodies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        for body in bodies {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 8192];
            let request = loop {
                let read = socket.read(&mut chunk).await.unwrap();
                buffer.extend_from_slice(&chunk[..read]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if rest.len() >= length {
                        break rest[..length].to_string();
                    }
                }
                if read == 0 {
                    break String::new();
                }
            };
            recorded.lock().unwrap().push(serde_json::from_str(&request).unwrap_or(Value::Null));
            let status = if body.starts_with("ERROR") { "500 Internal Server Error" } else { "200 OK" };
            let content_type = if body.starts_with("ERROR") { "application/json" } else { "text/event-stream" };
            let body = body.trim_start_matches("ERROR");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        }
    });
    (url, requests)
}

fn sse(chunks: &[Value]) -> String {
    chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

fn tool_call_body(id: &str, name: &str, arguments: &str) -> String {
    let (first, second) = arguments.split_at(arguments.len() / 2);
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": first}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": second}}]}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}}),
    ])
}

fn text_body(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 6, "total_tokens": 36}}),
    ])
}

fn setup(root: &Path, url: &str) {
    std::fs::create_dir_all(root.join("agent")).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let models = json!({"providers": {"mock": {"baseUrl": url, "api": "openai-completions", "apiKey": "secret-key",
        "models": [{"id": "mock-model", "contextWindow": 100000, "maxTokens": 4000}]}}});
    std::fs::write(root.join("agent/models.json"), models.to_string()).unwrap();
}

fn pi(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_pi"))
        .args(args)
        .current_dir(root.join("work"))
        .env("PI_CODING_AGENT_DIR", root.join("agent"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(input) = stdin {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    }
    child.wait_with_output().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn print_mode_runs_tools_and_saves_the_session() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![
        tool_call_body("call_1", "bash", r#"{"command":"echo from-bash > out.txt && cat out.txt"}"#),
        text_body("Done: from-bash"),
    ])
    .await;
    setup(root.path(), &url);
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || pi(&root, &["--model", "mock/mock-model", "-p", "Run the command"], None)
    })
    .await
    .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "Done: from-bash");
    assert_eq!(std::fs::read_to_string(root.path().join("work/out.txt")).unwrap().trim(), "from-bash");

    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["model"], "mock-model");
    assert_eq!(requests[0]["stream"], true);
    let tool_names: Vec<&str> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tool_names, ["read", "bash", "edit", "write"]);
    let tool_message =
        requests[1]["messages"].as_array().unwrap().iter().find(|message| message["role"] == "tool").unwrap();
    assert!(tool_message["content"].as_str().unwrap().contains("from-bash"));

    let sessions = root.path().join("agent/sessions");
    let dir = std::fs::read_dir(&sessions).unwrap().next().unwrap().unwrap().path();
    let file = std::fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let lines: Vec<Value> =
        std::fs::read_to_string(&file).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines[0]["type"], "session");
    assert!(lines.iter().any(|line| line["type"] == "model_change" && line["modelId"] == "mock-model"));
    assert_eq!(lines.iter().filter(|line| line["type"] == "message").count(), 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn json_mode_streams_events_and_reads_stdin_and_files() {
    let root = tempfile::tempdir().unwrap();
    let (url, requests) = mock_server(vec![text_body("Hello")]).await;
    setup(root.path(), &url);
    std::fs::write(root.path().join("work/context.txt"), "file contents").unwrap();
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || {
            pi(
                &root,
                &["--model", "mock-model", "--mode", "json", "--no-session", "-nt", "@context.txt", "-p", "Summarize"],
                Some("piped text"),
            )
        }
    })
    .await
    .unwrap();
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    let events: Vec<Value> =
        String::from_utf8_lossy(&output.stdout).lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"agent_start"));
    assert_eq!(kinds.last(), Some(&"agent_end"));
    assert!(kinds.contains(&"message_update"));
    let request = &requests.lock().unwrap()[0];
    assert!(request.get("tools").is_none());
    let user = request["messages"].as_array().unwrap().iter().find(|message| message["role"] == "user").unwrap();
    let text = user["content"].to_string();
    assert!(
        text.contains("context.txt")
            && text.contains("file contents")
            && text.contains("piped text")
            && text.contains("Summarize")
    );
    assert!(!root.path().join("agent/sessions").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_provider_errors_with_a_failing_exit_code() {
    let root = tempfile::tempdir().unwrap();
    let (url, _) = mock_server(vec![r#"ERROR{"error":{"message":"upstream exploded"}}"#.to_string()]).await;
    setup(root.path(), &url);
    let output = tokio::task::spawn_blocking({
        let root = root.path().to_path_buf();
        move || pi(&root, &["--model", "mock/mock-model", "--no-session", "-p", "hi"], None)
    })
    .await
    .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("500") && stderr.contains("upstream exploded"), "{stderr}");
}

#[test]
fn rejects_unknown_flags_and_lists_models() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path(), "http://127.0.0.1:9/v1");
    let output = pi(root.path(), &["--bogus"], None);
    assert_eq!(output.status.code(), Some(2));
    let output = pi(root.path(), &["--list-models", "mock"], None);
    assert!(String::from_utf8_lossy(&output.stdout).contains("mock/mock-model"));
}
