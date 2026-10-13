//! A one-request-per-connection HTTP/1.1 listener for OAuth redirects. It reads only the
//! request line and headers, which is all a browser redirect carries.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const MAX_HEAD_BYTES: usize = 64 * 1024;

/// The parts of a request the callback handlers look at.
#[derive(Clone, Debug)]
pub(crate) struct LoopbackRequest {
    pub method: String,
    /// The request target resolved against `http://localhost`, as `new URL(request.url, base)`.
    pub url: Url,
}

pub(crate) struct LoopbackResponse {
    pub status: u16,
    pub headers: Vec<(&'static str, &'static str)>,
    pub body: String,
}

impl LoopbackResponse {
    /// An HTML page with `content-type: text/html; charset=utf-8` and `cache-control: no-store`.
    pub fn page(status: u16, body: String) -> Self {
        Self {
            status,
            headers: vec![("content-type", "text/html; charset=utf-8"), ("cache-control", "no-store")],
            body,
        }
    }
}

pub(crate) type LoopbackHandler = Arc<dyn Fn(LoopbackRequest) -> BoxFuture<'static, LoopbackResponse> + Send + Sync>;

/// A listening loopback server. Dropping or closing it stops accepting connections and
/// drops connections that have not sent a request yet; requests already being handled finish.
pub(crate) struct LoopbackServer {
    pub port: u16,
    shutdown: CancellationToken,
}

impl LoopbackServer {
    pub async fn bind(host: &str, port: u16, handler: LoopbackHandler) -> io::Result<Self> {
        let listener = TcpListener::bind((host, port)).await?;
        let port = listener.local_addr()?.port();
        let shutdown = CancellationToken::new();
        tokio::spawn(accept_loop(listener, handler, shutdown.clone()));
        Ok(Self { port, shutdown })
    }

    pub fn close(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.close();
    }
}

async fn accept_loop(listener: TcpListener, handler: LoopbackHandler, shutdown: CancellationToken) {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(serve_connection(stream, handler.clone(), shutdown.clone()));
            }
            // Transient failures such as running out of file descriptors.
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

async fn serve_connection(mut stream: TcpStream, handler: LoopbackHandler, shutdown: CancellationToken) {
    let head = tokio::select! {
        _ = shutdown.cancelled() => return,
        head = read_head(&mut stream) => head,
    };
    let Some(request) = head.as_deref().and_then(parse_request) else {
        let _ = stream.shutdown().await;
        return;
    };
    let response = handler(request).await;
    let _ = write_response(&mut stream, &response).await;
}

async fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            buffer.truncate(end);
            return Some(buffer);
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn parse_request(head: &[u8]) -> Option<LoopbackRequest> {
    let head = std::str::from_utf8(head).ok()?;
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let url = Url::parse("http://localhost/").ok()?.join(target).ok()?;
    Some(LoopbackRequest { method, url })
}

async fn write_response(stream: &mut TcpStream, response: &LoopbackResponse) -> io::Result<()> {
    let reason = reqwest::StatusCode::from_u16(response.status).ok().and_then(|code| code.canonical_reason());
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status, reason.unwrap_or(""));
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", response.body.len()));
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(response.body.as_bytes()).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// `http://host:port/path`, with IPv6 hosts in brackets.
pub(crate) fn loopback_uri(host: &str, port: u16, path: &str) -> String {
    if host.contains(':') { format!("http://[{host}]:{port}{path}") } else { format!("http://{host}:{port}{path}") }
}
