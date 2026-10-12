//! Route-based HTTP/1.1 mock server for provider tests: every connection gets one reply
//! chosen by a handler, and every request is recorded with its raw bytes. `mock_http`
//! serves a fixed reply script instead, with silent and slow bodies.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Debug)]
pub(crate) struct Recorded {
    pub method: String,
    pub path: String,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Written one at a time with a flush between them.
    pub chunks: Vec<Vec<u8>>,
    /// Keep the connection open after the last chunk.
    pub hang: bool,
}

impl Reply {
    pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            chunks: vec![body.into()],
            hang: false,
        }
    }

    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::new(status, "application/json", body.to_string())
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

pub(crate) struct RouteServer {
    pub base_url: String,
    pub requests: Arc<Mutex<Vec<Recorded>>>,
}

impl RouteServer {
    pub async fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let handler = Arc::new(handler);
        let recorded = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let (handler, recorded) = (handler.clone(), recorded.clone());
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut socket).await else { return };
                    let reply = handler(&request);
                    recorded.lock().unwrap().push(request);
                    write_reply(&mut socket, reply).await;
                });
            }
        });
        Self { base_url, requests }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn write_reply(socket: &mut tokio::net::TcpStream, reply: Reply) {
    let mut head = format!("HTTP/1.1 {} Mock\r\nConnection: close\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !reply.hang {
        head.push_str(&format!("Content-Length: {}\r\n", reply.chunks.iter().map(Vec::len).sum::<usize>()));
    }
    head.push_str("\r\n");
    let _ = socket.write_all(head.as_bytes()).await;
    for chunk in &reply.chunks {
        let _ = socket.write_all(chunk).await;
        let _ = socket.flush().await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    if reply.hang {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let _ = socket.shutdown().await;
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Recorded> {
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
    let (method, path) = (request_line.next()?.to_string(), request_line.next()?.to_string());
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
    Some(Recorded { method, path, headers, body: buffer[header_end..].to_vec() })
}
