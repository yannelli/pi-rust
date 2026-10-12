//! Scripted HTTP/1.1 server for adapter tests: one reply per connection, in order,
//! with custom status, headers, and streamed or stalled bodies.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(crate) enum Body {
    /// Write the chunks, then close.
    Chunks(Vec<String>),
    /// Write the chunks, then keep the connection open.
    ChunksThenHang(Vec<String>),
    /// Never send the response head.
    Silent,
    /// Write the chunks with a pause before each, then close.
    Slow(Vec<String>, Duration),
}

pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Body,
}

impl Reply {
    pub fn sse(frames: Vec<String>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Body::Chunks(frames),
        }
    }

    pub fn json(status: u16, body: &Value) -> Self {
        let headers = vec![("content-type".into(), "application/json".into())];
        Self { status, headers, body: Body::Chunks(vec![body.to_string()]) }
    }

    pub fn text(status: u16, content_type: &str, body: &str) -> Self {
        let headers = vec![("content-type".into(), content_type.into())];
        Self { status, headers, body: Body::Chunks(vec![body.to_string()]) }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub method: String,
    /// Path with query string.
    pub path: String,
    /// Lowercase header names.
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Request {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Decoded `application/x-www-form-urlencoded` fields.
    pub fn form(&self) -> HashMap<String, String> {
        let decode = |text: &str| {
            let bytes = text.replace('+', " ").into_bytes();
            let mut out = Vec::new();
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'%' && index + 2 < bytes.len() {
                    let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("00");
                    out.push(u8::from_str_radix(hex, 16).unwrap_or(0));
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            String::from_utf8_lossy(&out).into_owned()
        };
        let pairs = self.body.split('&').filter_map(|pair| pair.split_once('='));
        pairs.map(|(key, value)| (decode(key), decode(value))).collect()
    }
}

pub(crate) struct MockHttp {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MockHttp {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        tokio::spawn(async move {
            for reply in replies {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let Some(request) = read_request(&mut socket).await else { return };
                recorded.lock().unwrap().push(request);
                tokio::spawn(async move { write_reply(&mut socket, reply).await });
            }
        });
        Self { base_url, requests }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request(&self, index: usize) -> Request {
        self.requests()[index].clone()
    }
}

async fn write_reply(socket: &mut TcpStream, reply: Reply) {
    let (chunks, hang, pause) = match reply.body {
        Body::Silent => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        Body::Chunks(chunks) => (chunks, false, Duration::ZERO),
        Body::ChunksThenHang(chunks) => (chunks, true, Duration::ZERO),
        Body::Slow(chunks, pause) => (chunks, false, pause),
    };
    let mut head = format!("HTTP/1.1 {} Status\r\nConnection: close\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    for chunk in chunks {
        tokio::time::sleep(pause).await;
        let _ = socket.write_all(chunk.as_bytes()).await;
        let _ = socket.flush().await;
    }
    if hang {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let _ = socket.shutdown().await;
}

async fn read_request(socket: &mut TcpStream) -> Option<Request> {
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
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
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
    let body = String::from_utf8_lossy(&buffer[header_end..]).into_owned();
    Some(Request { method, path, headers, body })
}
