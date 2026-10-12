//! Minimal HTTP/1.1 server for provider tests. Each server accepts one request.

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream};
use crate::types::{AssistantMessage, Model, ModelCost};

pub(crate) enum MockResponse {
    /// Send these SSE frames, then close the connection.
    Sse(Vec<String>),
    /// Send these SSE frames, then keep the connection open.
    SseThenHang(Vec<String>),
    /// Reply with a status code and a JSON body.
    Status(u16, String),
}

#[derive(Debug)]
pub(crate) struct RecordedRequest {
    pub path: String,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
    pub body: Value,
}

pub(crate) struct MockServer {
    pub base_url: String,
    request: Option<oneshot::Receiver<RecordedRequest>>,
}

impl MockServer {
    pub async fn start(response: MockResponse) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let (sender, receiver) = oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let Some(request) = read_request(&mut socket).await else { return };
            let _ = sender.send(request);
            match response {
                MockResponse::Sse(frames) => {
                    write_sse(&mut socket, &frames).await;
                    let _ = socket.shutdown().await;
                }
                MockResponse::SseThenHang(frames) => {
                    write_sse(&mut socket, &frames).await;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                MockResponse::Status(status, body) => {
                    let head = format!(
                        "HTTP/1.1 {status} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            }
        });
        Self { base_url, request: Some(receiver) }
    }

    /// The request the server received.
    pub async fn request(&mut self) -> RecordedRequest {
        self.request.take().expect("request already taken").await.expect("server received a request")
    }
}

async fn write_sse(socket: &mut tokio::net::TcpStream, frames: &[String]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    let _ = socket.write_all(head.as_bytes()).await;
    for frame in frames {
        let _ = socket.write_all(frame.as_bytes()).await;
        let _ = socket.flush().await;
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.lines();
    let path = lines.next()?.split_whitespace().nth(1)?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or_default();
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null);
    Some(RecordedRequest { path, headers, body })
}

/// One SSE frame with a JSON data payload and an optional event name.
pub(crate) fn frame(event: Option<&str>, data: &Value) -> String {
    match event {
        Some(event) => format!("event: {event}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

pub(crate) fn test_model(api: &str, provider: &str, base_url: &str) -> Model {
    Model {
        id: "test-model".into(),
        name: "Test".into(),
        api: api.into(),
        provider: provider.into(),
        base_url: base_url.into(),
        reasoning: true,
        input: vec!["text".into(), "image".into()],
        cost: ModelCost { input: 1.0, output: 2.0, cache_read: 0.5, cache_write: 4.0, ..Default::default() },
        context_window: 100_000,
        max_tokens: 4096,
        headers: Default::default(),
        compat: None,
        ..Default::default()
    }
}

/// Collect all events and return them with the final message.
pub(crate) async fn collect(stream: AssistantMessageEventStream) -> (Vec<AssistantMessageEvent>, AssistantMessage) {
    let events: Vec<AssistantMessageEvent> = stream.collect().await;
    let last = events.last().and_then(AssistantMessageEvent::final_message).cloned().expect("terminal event");
    (events, last)
}

/// Event type names, for ordering assertions.
pub(crate) fn kinds(events: &[AssistantMessageEvent]) -> Vec<String> {
    events.iter().map(|event| serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string()).collect()
}

/// Cancel `cancel` at the first text delta and return the final message.
pub(crate) async fn cancel_after_first_text(
    mut stream: AssistantMessageEventStream,
    cancel: tokio_util::sync::CancellationToken,
) -> AssistantMessage {
    let drive = async {
        while let Some(event) = stream.next().await {
            if let AssistantMessageEvent::TextDelta { .. } = event {
                cancel.cancel();
            }
            if let Some(message) = event.final_message() {
                return message.clone();
            }
        }
        panic!("stream ended without a terminal event");
    };
    tokio::time::timeout(Duration::from_secs(5), drive).await.expect("cancellation is prompt")
}
