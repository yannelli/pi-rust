//! Local Acme provider fixtures: an OpenAI-compatible SSE endpoint and a custom NDJSON
//! streaming endpoint, each recording whether it received the exact probe request.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

pub const OPENAI_PROVIDER_ID: &str = "acme";
pub const OPENAI_MODEL_ID: &str = "acme-chat";
pub const OPENAI_PROBE_PROMPT: &str = "Reply with ACME_OK.";
pub const OPENAI_PROBE_RESPONSE: &str = "ACME_OK";
pub const STREAM_PROVIDER_ID: &str = "acme-stream";
pub const STREAM_MODEL_ID: &str = "acme-stream-chat";
pub const STREAM_PROBE_PROMPT: &str = "Reply with ACME_STREAM_OK.";
pub const STREAM_PROBE_RESPONSE: &str = "ACME_STREAM_OK";

/// The API description the custom-provider eval seeds into the workspace.
pub fn stream_api_documentation() -> String {
    let documentation = json!({
        "name": "Acme Streaming API",
        "request": {
            "method": "POST",
            "path": "/generate",
            "headers": { "content-type": "application/json", "x-acme-key": "resolved credential" },
            "body": { "model": STREAM_MODEL_ID, "messages": [{ "role": "user", "content": "Hello" }], "stream": true },
        },
        "response": {
            "contentType": "application/x-ndjson",
            "events": [
                { "type": "text_delta", "text": "Hello" },
                { "type": "usage", "input_tokens": 3, "output_tokens": 2 },
                { "type": "done", "reason": "stop" },
            ],
        },
    });
    format!("{}\n", serde_json::to_string_pretty(&documentation).unwrap_or_default())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMode {
    OpenAi,
    Stream,
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
}

fn reject(status: u16, message: &str) -> Response {
    Response { status, content_type: "text/plain", body: message.to_string() }
}

fn handle(mode: ServerMode, request: &Request, received: &AtomicBool) -> Response {
    let expected_path = if mode == ServerMode::OpenAi { "/v1/chat/completions" } else { "/generate" };
    if request.path != expected_path {
        return reject(404, "Unknown endpoint");
    }
    if request.method != "POST" {
        return reject(405, "Expected POST");
    }
    if !request.headers.get("content-type").is_some_and(|value| value.starts_with("application/json")) {
        return reject(415, "Expected application/json");
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&request.body) else { return reject(400, "Invalid JSON") };
    if !payload.is_object() {
        return reject(422, "Expected a JSON object");
    }
    let messages = payload["messages"].as_array().cloned().unwrap_or_default();
    let user = messages.iter().find(|message| message.is_object() && message["role"] == "user");
    let prompt = user.and_then(|message| message["content"].as_str());
    let header = |name: &str| request.headers.get(name).map(String::as_str);
    if mode == ServerMode::Stream {
        if header("x-acme-key") != Some("resolved-stream-key") {
            return reject(401, "Invalid Acme Stream credential");
        }
        if payload["model"] != STREAM_MODEL_ID || prompt.is_none() || payload["stream"] != true {
            return reject(422, "Invalid Acme Stream request");
        }
        received.store(prompt == Some(STREAM_PROBE_PROMPT), Ordering::SeqCst);
        let events = [
            json!({ "type": "text_delta", "text": "ACME_" }),
            json!({ "type": "text_delta", "text": "STREAM_OK" }),
            json!({ "type": "usage", "input_tokens": 4, "output_tokens": 3 }),
            json!({ "type": "done", "reason": "stop" }),
        ];
        let body = events.iter().map(|event| format!("{event}\n")).collect();
        return Response { status: 200, content_type: "application/x-ndjson", body };
    }
    if header("authorization") != Some("Bearer resolved-acme-key") {
        return reject(401, "Invalid Acme credential");
    }
    if payload["model"] != OPENAI_MODEL_ID || prompt.is_none() || payload["stream"] != true {
        return reject(422, "Invalid OpenAI-compatible request");
    }
    received.store(prompt == Some(OPENAI_PROBE_PROMPT), Ordering::SeqCst);
    let chunk = |delta: Value, finish: Value, usage: Option<Value>| {
        let mut chunk = json!({
            "id": "chatcmpl-acme", "object": "chat.completion.chunk", "created": 0, "model": OPENAI_MODEL_ID,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });
        if let Some(usage) = usage {
            chunk["usage"] = usage;
        }
        format!("data: {chunk}\n\n")
    };
    let body = [
        chunk(json!({ "role": "assistant", "content": OPENAI_PROBE_RESPONSE }), Value::Null, None),
        chunk(json!({}), json!("stop"), Some(json!({ "prompt_tokens": 3, "completion_tokens": 2 }))),
        "data: [DONE]\n\n".to_string(),
    ]
    .concat();
    Response { status: 200, content_type: "text/event-stream", body }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, path) = (request_line.next()?.to_string(), request_line.next()?.to_string());
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers.get("content-length").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Some(Request { method, path, headers, body })
}

async fn serve(mut stream: TcpStream, mode: ServerMode, received: Arc<AtomicBool>) {
    let Some(request) = read_request(&mut stream).await else { return };
    let response = handle(mode, &request, &received);
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        415 => "Unsupported Media Type",
        _ => "Unprocessable Entity",
    };
    let cache = if response.content_type == "text/event-stream" { "cache-control: no-cache\r\n" } else { "" };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\ncontent-type: {}\r\n{cache}content-length: {}\r\nconnection: close\r\n\r\n",
        response.status,
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(response.body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// A fixture server bound to an ephemeral loopback port while started.
pub struct AcmeServer {
    mode: ServerMode,
    received: Arc<AtomicBool>,
    origin: Mutex<String>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl AcmeServer {
    pub fn new(mode: ServerMode) -> Self {
        Self { mode, received: Arc::default(), origin: Mutex::default(), task: Mutex::default() }
    }

    pub async fn start(&self) -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        *self.origin.lock().unwrap_or_else(|e| e.into_inner()) =
            format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let (mode, received) = (self.mode, self.received.clone());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, mode, received.clone()));
            }
        });
        *self.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        Ok(())
    }

    pub fn stop(&self) {
        if let Some(task) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            task.abort();
        }
    }

    pub fn reset(&self) {
        self.received.store(false, Ordering::SeqCst);
    }

    pub fn origin(&self) -> String {
        self.origin.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn base_url(&self) -> String {
        format!("{}/v1", self.origin())
    }

    pub fn valid_request_received(&self) -> bool {
        self.received.load(Ordering::SeqCst)
    }

    /// A shareable probe of [`Self::valid_request_received`].
    pub fn probe(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let received = self.received.clone();
        Arc::new(move || received.load(Ordering::SeqCst))
    }
}

impl Drop for AcmeServer {
    fn drop(&mut self) {
        self.stop();
    }
}
