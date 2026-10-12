//! Test doubles: a routing mock HTTP server and a scripted login interaction.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::oauth::http::Hooks;
use crate::auth::{AuthError, AuthEvent, AuthInteraction, AuthPrompt, AuthResult};

/// One request the mock server received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    /// Path without the query; rewritten provider URLs start with their original host.
    pub path: String,
    /// Header names are lowercase.
    pub headers: HashMap<String, String>,
    pub body: String,
    pub at: Instant,
}

impl Recorded {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("JSON request body")
    }

    pub fn form(&self, name: &str) -> Option<String> {
        crate::auth::oauth::http::search_param(&self.body, name)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

pub enum Reply {
    Json(u16, Value),
    JsonHeaders(u16, Value, Vec<(&'static str, String)>),
    Text(u16, String),
    /// Reply once the sender fires.
    Later(oneshot::Receiver<Reply>),
    /// Close the connection without a response.
    Close,
}

type Handler = Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>;

pub struct MockServer {
    pub base: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    shutdown: CancellationToken,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl MockServer {
    pub async fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock server");
        let base = format!("http://{}", listener.local_addr().expect("local address"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = CancellationToken::new();
        let (handler, log, stop): (Handler, _, _) = (Arc::new(handler), requests.clone(), shutdown.clone());
        tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    _ = stop.cancelled() => return,
                    accepted = listener.accept() => match accepted { Ok(accepted) => accepted, Err(_) => return },
                };
                tokio::spawn(serve(stream, handler.clone(), log.clone()));
            }
        });
        Self { base, requests, shutdown }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn requests_to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests().into_iter().filter(|request| request.path.ends_with(suffix)).collect()
    }

    /// Routes `https://host/path` to `{base}/host/path`.
    pub fn hooks(&self) -> Hooks {
        let base = self.base.clone();
        let rewrite = Arc::new(move |url: &str| {
            let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
            format!("{base}/{rest}")
        });
        Hooks { rewrite: Some(rewrite), env: Some(Arc::new(|_| None)), no_timeouts: true }
    }
}

async fn serve(mut stream: TcpStream, handler: Handler, log: Arc<Mutex<Vec<Recorded>>>) {
    let Some(request) = read_request(&mut stream).await else { return };
    log.lock().unwrap().push(request.clone());
    let mut reply = handler(&request);
    if let Reply::Later(receiver) = reply {
        reply = receiver.await.unwrap_or(Reply::Close);
    }
    let (status, body, headers) = match reply {
        Reply::Json(status, body) => (status, body.to_string(), vec![("content-type", "application/json".into())]),
        Reply::JsonHeaders(status, body, mut headers) => {
            headers.push(("content-type", "application/json".into()));
            (status, body.to_string(), headers)
        }
        Reply::Text(status, body) => (status, body, vec![("content-type", "text/plain".into())]),
        Reply::Later(_) | Reply::Close => return,
    };
    let mut head = format!("HTTP/1.1 {status} Mock\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", body.len()));
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let head_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut start = lines.next()?.split_whitespace();
    let method = start.next()?.to_string();
    let target = start.next()?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers.get("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    while buffer.len() < head_end + length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();
    let path = target.split('?').next().unwrap_or_default().to_string();
    Some(Recorded { method, path, headers, body, at: Instant::now() })
}

/// A JSON reply sequence for repeated polls; panics when exhausted.
pub fn sequence(replies: Vec<(u16, Value)>) -> impl Fn() -> Reply + Send + Sync {
    let replies = Mutex::new(std::collections::VecDeque::from(replies));
    move || {
        let (status, body) = replies.lock().unwrap().pop_front().expect("unexpected extra request");
        Reply::Json(status, body)
    }
}

type PromptFn =
    dyn Fn(AuthPrompt, CancellationToken, Vec<AuthEvent>) -> BoxFuture<'static, AuthResult<String>> + Send + Sync;

/// Records events and prompts, and answers prompts with a script.
pub struct TestInteraction {
    pub events: Mutex<Vec<AuthEvent>>,
    pub prompts: Mutex<Vec<(AuthPrompt, CancellationToken)>>,
    answer: Box<PromptFn>,
    on_notify: Box<dyn Fn(&AuthEvent) + Send + Sync>,
}

impl TestInteraction {
    pub fn new(
        answer: impl Fn(AuthPrompt, CancellationToken, Vec<AuthEvent>) -> BoxFuture<'static, AuthResult<String>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            events: Mutex::default(),
            prompts: Mutex::default(),
            answer: Box::new(answer),
            on_notify: Box::new(|_| {}),
        }
    }

    pub fn on_notify(mut self, on_notify: impl Fn(&AuthEvent) + Send + Sync + 'static) -> Self {
        self.on_notify = Box::new(on_notify);
        self
    }

    pub fn events(&self) -> Vec<AuthEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn auth_url(&self) -> reqwest::Url {
        auth_url(&self.events()).expect("auth_url event")
    }
}

#[async_trait]
impl AuthInteraction for TestInteraction {
    async fn prompt(&self, prompt: AuthPrompt, cancel: CancellationToken) -> AuthResult<String> {
        self.prompts.lock().unwrap().push((prompt.clone(), cancel.clone()));
        (self.answer)(prompt, cancel, self.events()).await
    }

    fn notify(&self, event: AuthEvent) {
        (self.on_notify)(&event);
        self.events.lock().unwrap().push(event);
    }
}

pub fn auth_url(events: &[AuthEvent]) -> Option<reqwest::Url> {
    events.iter().rev().find_map(|event| match event {
        AuthEvent::AuthUrl { url, .. } => reqwest::Url::parse(url).ok(),
        _ => None,
    })
}

pub fn param(url: &reqwest::Url, name: &str) -> Option<String> {
    crate::auth::oauth::http::url_param(url, name)
}

/// A prompt that stays open until its token is cancelled.
pub fn pending(cancel: CancellationToken) -> BoxFuture<'static, AuthResult<String>> {
    Box::pin(async move {
        cancel.cancelled().await;
        Err(AuthError::other("prompt aborted"))
    })
}

pub fn answer(text: impl Into<String>) -> BoxFuture<'static, AuthResult<String>> {
    let text = text.into();
    Box::pin(async move { Ok(text) })
}

/// A port that was free a moment ago.
pub async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Sends a GET to a loopback URL and returns the status and body.
pub async fn get(url: &str) -> reqwest::Result<(u16, String)> {
    let response = reqwest::Client::new().get(url).send().await?;
    Ok((response.status().as_u16(), response.text().await?))
}

pub fn expect_err<T: std::fmt::Debug>(result: AuthResult<T>) -> String {
    result.expect_err("expected an error").to_string()
}
