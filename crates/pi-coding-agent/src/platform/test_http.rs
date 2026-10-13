//! Local HTTP/1.1 server for tests: one request per connection, answered by a handler,
//! with every request recorded. Streaming replies send chunks until the sender drops.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub(crate) struct TestRequest {
    pub method: String,
    /// Path and query.
    pub target: String,
    /// Lowercase header names.
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl TestRequest {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn query(&self, name: &str) -> Option<String> {
        let url = reqwest::Url::parse(&format!("http://localhost{}", self.target)).ok()?;
        url.query_pairs().find(|(key, _)| key == name).map(|(_, value)| value.into_owned())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

pub(crate) enum TestReply {
    Full {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// An event stream whose body ends when the sender is dropped.
    Stream(mpsc::UnboundedReceiver<Vec<u8>>),
}

impl TestReply {
    pub fn json(value: &serde_json::Value) -> Self {
        Self::status(200, &value.to_string()).header("content-type", "application/json")
    }

    pub fn status(status: u16, body: &str) -> Self {
        Self::bytes(status, body.as_bytes().to_vec())
    }

    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self::Full { status, headers: Vec::new(), body }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Self::Full { headers, .. } = &mut self {
            headers.push((name.into(), value.into()));
        }
        self
    }
}

type Handler = dyn Fn(&TestRequest) -> TestReply + Send + Sync;

pub(crate) struct TestServer {
    pub url: String,
    requests: Arc<Mutex<Vec<TestRequest>>>,
}

impl TestServer {
    pub async fn start(handler: impl Fn(&TestRequest) -> TestReply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Arc<Handler> = Arc::new(handler);
        let recorded = requests.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let (handler, recorded) = (handler.clone(), recorded.clone());
                tokio::spawn(async move {
                    let _ = serve(socket, handler, recorded).await;
                });
            }
        });
        Self { url, requests }
    }

    pub fn requests(&self) -> Vec<TestRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self, path: &str) -> usize {
        self.requests().iter().filter(|request| request.path() == path).count()
    }
}

/// A client that ignores proxy variables, so requests reach the local server.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Status",
    }
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<Option<TestRequest>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break index;
        }
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let method = start.next().unwrap_or_default().to_string();
    let target = start.next().unwrap_or_default().to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length = headers.get("content-length").and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Some(TestRequest { method, target, headers, body }))
}

async fn serve(
    mut socket: TcpStream,
    handler: Arc<Handler>,
    recorded: Arc<Mutex<Vec<TestRequest>>>,
) -> std::io::Result<()> {
    let Some(request) = read_request(&mut socket).await? else { return Ok(()) };
    recorded.lock().unwrap().push(request.clone());
    match handler(&request) {
        TestReply::Full { status, headers, body } => {
            let mut head = format!("HTTP/1.1 {status} {}\r\nconnection: close\r\n", reason(status));
            for (name, value) in &headers {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
            head.push_str(&format!("content-length: {}\r\n\r\n", body.len()));
            socket.write_all(head.as_bytes()).await?;
            socket.write_all(&body).await?;
        }
        TestReply::Stream(mut receiver) => {
            let head = "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\n\r\n";
            socket.write_all(head.as_bytes()).await?;
            socket.flush().await?;
            while let Some(chunk) = receiver.recv().await {
                socket.write_all(&chunk).await?;
                socket.flush().await?;
            }
        }
    }
    socket.shutdown().await
}
