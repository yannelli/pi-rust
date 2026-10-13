//! Minimal HTTP/1.1 server for tests: one request per connection, handled by a closure.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub struct Request {
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    /// Lowercase names.
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Request {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    pub fn query(&self) -> HashMap<String, String> {
        let query = self.target.split_once('?').map(|(_, query)| query).unwrap_or_default();
        url::form_urlencoded::parse(query.as_bytes()).into_owned().collect()
    }

    pub fn form(&self) -> HashMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes()).into_owned().collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub enum Response {
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// Chunks written as they arrive; the connection closes when the sender is dropped.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: mpsc::UnboundedReceiver<String>,
    },
    /// Never answer.
    Hang,
}

impl Response {
    pub fn status(status: u16) -> Self {
        Self::Full { status, headers: Vec::new(), body: String::new() }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::Full { status, headers: Vec::new(), body: body.into() }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::Full {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string(),
        }
    }

    pub fn sse(body: impl Into<String>) -> Self {
        Self::Full {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: body.into(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        match &mut self {
            Self::Full { headers, .. } | Self::Stream { headers, .. } => headers.push((name.into(), value.into())),
            Self::Hang => {}
        }
        self
    }
}

pub type Handler = Arc<dyn Fn(Request, String) -> BoxFuture<'static, Response> + Send + Sync>;

/// Start a server on 127.0.0.1 and return its origin. The handler gets the request and the origin.
pub async fn serve<F, Fut>(handler: F) -> String
where
    F: Fn(Request, String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    let handler: Handler = Arc::new(move |request, origin| Box::pin(handler(request, origin)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let base = origin.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(handle(socket, handler.clone(), base.clone()));
        }
    });
    origin
}

async fn read_request(socket: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buffer = [0u8; 8192];
    let head_end = loop {
        if let Some(index) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        data.extend_from_slice(&buffer[..read]);
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_string();
    let target = request_line.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut buffer).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Some(Request { method, target, headers, body: String::from_utf8_lossy(&body).into_owned() })
}

fn head(status: u16, headers: &[(String, String)], length: Option<usize>) -> String {
    let mut text = format!("HTTP/1.1 {status} Status\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(length) = length {
        text.push_str(&format!("Content-Length: {length}\r\n"));
    }
    text.push_str("\r\n");
    text
}

async fn handle(mut socket: TcpStream, handler: Handler, origin: String) {
    let Some(request) = read_request(&mut socket).await else { return };
    match handler(request, origin).await {
        Response::Full { status, headers, body } => {
            let _ = socket.write_all(head(status, &headers, Some(body.len())).as_bytes()).await;
            let _ = socket.write_all(body.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
        Response::Stream { status, headers, mut chunks } => {
            let _ = socket.write_all(head(status, &headers, None).as_bytes()).await;
            while let Some(chunk) = chunks.recv().await {
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
            }
            let _ = socket.shutdown().await;
        }
        Response::Hang => tokio::time::sleep(Duration::from_secs(600)).await,
    }
}
