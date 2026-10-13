//! Shared helpers for `pi` binary tests: a scripted OpenAI-compatible server and runners.
#![allow(dead_code)]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves scripted SSE bodies in order and records each request body.
pub async fn mock_server(bodies: Vec<String>) -> (String, Arc<Mutex<Vec<Value>>>) {
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

pub fn sse(chunks: &[Value]) -> String {
    chunks.iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>() + "data: [DONE]\n\n"
}

pub fn tool_call_body(id: &str, name: &str, arguments: &str) -> String {
    let (first, second) = arguments.split_at(arguments.len() / 2);
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": first}}]}}]}),
        json!({"choices": [{"index": 0, "delta": {"tool_calls": [{"index": 0, "function": {"arguments": second}}]}, "finish_reason": "tool_calls"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 20, "completion_tokens": 5, "total_tokens": 25}}),
    ])
}

pub fn text_body(text: &str) -> String {
    sse(&[
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 6, "total_tokens": 36}}),
    ])
}

pub fn setup(root: &Path, url: &str) {
    std::fs::create_dir_all(root.join("agent")).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    let models = json!({"providers": {"mock": {"baseUrl": url, "api": "openai-completions", "apiKey": "secret-key",
        "models": [{"id": "mock-model", "contextWindow": 100000, "maxTokens": 4000}]}}});
    std::fs::write(root.join("agent/models.json"), models.to_string()).unwrap();
}

pub fn pi(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
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

pub fn write_settings(root: &Path, settings: Value) {
    std::fs::write(root.join("agent/settings.json"), settings.to_string()).unwrap();
}

pub async fn pi_async(root: &Path, args: &[&str]) -> Output {
    let root = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
    tokio::task::spawn_blocking(move || pi(&root, &args.iter().map(String::as_str).collect::<Vec<_>>(), None))
        .await
        .unwrap()
}

pub fn events(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout).lines().map(|line| serde_json::from_str(line).unwrap()).collect()
}
