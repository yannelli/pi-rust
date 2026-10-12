//! Minimal HTTP/1.1 server for provider tests. A server answers its scripted responses in
//! order, one per connection, and records every request it receives.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream};
use crate::types::{AssistantMessage, Model, ModelCost};

#[derive(Clone, Debug)]
pub(crate) enum MockResponse {
    /// Send these SSE frames, then close the connection.
    Sse(Vec<String>),
    /// Send these SSE frames, then keep the connection open.
    SseThenHang(Vec<String>),
    /// Reply with a status code and a JSON body.
    Status(u16, String),
    /// Reply with a status code, extra headers, and a body.
    Full { status: u16, headers: Vec<(String, String)>, body: String },
    /// Wait this long before answering with the inner response.
    Delayed(Duration, Box<MockResponse>),
}

impl MockResponse {
    /// A 200 response with a JSON body.
    pub fn json(body: &Value) -> Self {
        Self::Status(200, body.to_string())
    }

    pub fn with_headers(status: u16, headers: &[(&str, &str)], body: impl Into<String>) -> Self {
        let headers = headers.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect();
        Self::Full { status, headers, body: body.into() }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RecordedRequest {
    /// Request target; an absolute URL when the request came through a proxy.
    pub path: String,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
    pub body: Value,
}

pub(crate) struct MockServer {
    pub base_url: String,
    requests: mpsc::UnboundedReceiver<RecordedRequest>,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockServer {
    /// A server that answers one request.
    pub async fn start(response: MockResponse) -> Self {
        Self::sequence(vec![response]).await
    }

    /// A server that answers `responses` in order, one per request.
    pub async fn sequence(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let (sender, requests) = mpsc::unbounded_channel();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let Some(request) = read_request(&mut socket).await else { return };
                log.lock().unwrap().push(request.clone());
                let _ = sender.send(request);
                tokio::spawn(async move { respond(&mut socket, response).await });
            }
        });
        Self { base_url, requests, received }
    }

    /// A server that answers any number of requests with `handler`.
    pub async fn routed(handler: impl Fn(&RecordedRequest) -> MockResponse + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let (sender, requests) = mpsc::unbounded_channel();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let Some(request) = read_request(&mut socket).await else { continue };
                let response = handler(&request);
                log.lock().unwrap().push(request.clone());
                let _ = sender.send(request);
                tokio::spawn(async move { respond(&mut socket, response).await });
            }
        });
        Self { base_url, requests, received }
    }

    /// The next request the server received, in order.
    pub async fn request(&mut self) -> RecordedRequest {
        tokio::time::timeout(Duration::from_secs(5), self.requests.recv())
            .await
            .expect("server received a request in time")
            .expect("server is running")
    }

    /// Every request received so far.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.received.lock().unwrap().clone()
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, response: MockResponse) {
    match response {
        MockResponse::Sse(frames) => {
            write_sse(socket, &frames).await;
            let _ = socket.shutdown().await;
        }
        MockResponse::SseThenHang(frames) => {
            write_sse(socket, &frames).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        MockResponse::Status(status, body) => write_full(socket, status, &[], &body).await,
        MockResponse::Full { status, headers, body } => write_full(socket, status, &headers, &body).await,
        MockResponse::Delayed(delay, inner) => {
            tokio::time::sleep(delay).await;
            Box::pin(respond(socket, *inner)).await;
        }
    }
}

async fn write_full(socket: &mut tokio::net::TcpStream, status: u16, headers: &[(String, String)], body: &str) {
    let mut head = format!("HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    if !headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type")) {
        head.push_str("Content-Type: application/json\r\n");
    }
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn write_sse(socket: &mut tokio::net::TcpStream, frames: &[String]) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nx-request-id: req_1\r\nConnection: close\r\n\r\n";
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
