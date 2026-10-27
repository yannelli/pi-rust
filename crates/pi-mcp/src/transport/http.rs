//! Streamable HTTP transport: POST for messages, SSE or JSON responses, and an optional GET stream.

mod streams;

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Response};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{DEFAULT_MAX_MESSAGE_BYTES, McpTransport, TransportSink};
use crate::auth::{AuthProvider, UnauthorizedContext};
use crate::error::McpError;
use crate::jsonrpc::JsonRpcMessage;

const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_MESSAGE_BODY_CHARS: usize = 500;

/// Reconnection of dropped SSE streams: the GET stream, and response streams with event IDs.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectOptions {
    /// Delay before the first try, unless the server sent a `retry` field. Default: 1 s.
    pub initial_delay: Duration,
    /// Upper bound of the exponential backoff. Default: 30 s.
    pub max_delay: Duration,
    /// Consecutive failed tries before giving up on a stream. Default: 5.
    pub max_retries: u32,
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self { initial_delay: Duration::from_secs(1), max_delay: Duration::from_secs(30), max_retries: 5 }
    }
}

#[derive(Clone)]
pub struct StreamableHttpOptions {
    pub url: Url,
    pub headers: Vec<(String, String)>,
    pub client: Option<reqwest::Client>,
    /// Open the server-to-client GET stream after initialization. Default: true.
    pub open_get_stream: bool,
    pub max_message_bytes: usize,
    pub auth_provider: Option<Arc<dyn AuthProvider>>,
    pub reconnect: ReconnectOptions,
}

impl StreamableHttpOptions {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            headers: Vec::new(),
            client: None,
            open_get_stream: true,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            auth_provider: None,
            reconnect: ReconnectOptions::default(),
        }
    }
}

#[derive(Default)]
struct State {
    started: bool,
    closed: bool,
    session_id: Option<String>,
    protocol_version: Option<String>,
    get_stream_started: bool,
    /// Access token of the latest request, which `close` reuses instead of asking the provider.
    last_token: Option<String>,
    sink: Option<TransportSink>,
}

pub(crate) struct Inner {
    url: Url,
    headers: HeaderMap,
    client: reqwest::Client,
    options: StreamableHttpOptions,
    cancel: CancellationToken,
    state: Mutex<State>,
}

pub struct StreamableHttpTransport {
    inner: Arc<Inner>,
}

impl StreamableHttpTransport {
    pub fn new(options: StreamableHttpOptions) -> Result<Self, McpError> {
        let mut headers = HeaderMap::new();
        for (name, value) in &options.headers {
            let header = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| McpError::other(format!("Invalid HTTP header name {name:?}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|_| McpError::other(format!("Invalid value for HTTP header {name:?}")))?;
            headers.insert(header, value);
        }
        let client = options.client.clone().unwrap_or_else(|| pi_ai::http_client().clone());
        let inner = Inner {
            url: options.url.clone(),
            headers,
            client,
            options,
            cancel: CancellationToken::new(),
            state: Mutex::default(),
        };
        Ok(Self { inner: Arc::new(inner) })
    }

    pub fn session_id(&self) -> Option<String> {
        self.inner.lock().session_id.clone()
    }
}

fn content_type(response: &Response) -> Option<String> {
    let value = response.headers().get(reqwest::header::CONTENT_TYPE)?.to_str().ok()?;
    Some(value.split(';').next().unwrap_or_default().trim().to_ascii_lowercase())
}

/// 401, or 403 with an `insufficient_scope` bearer challenge (step-up authorization).
pub(crate) fn needs_authorization(status: u16, headers: &HeaderMap) -> bool {
    if status == 401 {
        return true;
    }
    if status != 403 {
        return false;
    }
    let challenge = headers
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    challenge.match_indices("error=").any(|(index, _)| {
        let boundary = index == 0 || matches!(challenge.as_bytes()[index - 1], b' ' | b'\t' | b',');
        let rest = &challenge[index + 6..];
        boundary && rest.strip_prefix('"').unwrap_or(rest).starts_with("insufficient_scope")
    })
}

/// Reads the body in chunks and stops after `limit` bytes, so an oversized body is never buffered
/// whole. Returns the bytes read and whether the body was longer than `limit`.
async fn read_body_limited(response: &mut Response, limit: usize) -> Result<(Vec<u8>, bool), McpError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(McpError::from_reqwest)? {
        if chunk.len() > limit - body.len() {
            body.extend_from_slice(&chunk[..limit - body.len()]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

fn describe_http_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if text.chars().count() > ERROR_MESSAGE_BODY_CHARS {
        format!("{}...", text.chars().take(ERROR_MESSAGE_BODY_CHARS - 3).collect::<String>())
    } else {
        text.to_string()
    };
    if snippet.is_empty() {
        format!("MCP HTTP request failed with status {status}")
    } else {
        format!("MCP HTTP request failed with status {status}: {snippet}")
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn closed(&self) -> bool {
        self.lock().closed
    }

    fn sink(&self) -> Option<TransportSink> {
        self.lock().sink.clone()
    }

    fn build_headers(&self, extra: &[(&str, String)], token: Option<&str>) -> HeaderMap {
        let mut headers = self.headers.clone();
        for (name, value) in extra {
            if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
                headers.insert(name, value);
            }
        }
        let state = self.lock();
        let mut set = |name: &'static str, value: &str| {
            if let Ok(value) = HeaderValue::from_str(value) {
                headers.insert(name, value);
            }
        };
        if let Some(session) = &state.session_id {
            set("mcp-session-id", session);
        }
        if let Some(version) = &state.protocol_version {
            set("mcp-protocol-version", version);
        }
        if let Some(token) = token {
            set("authorization", &format!("Bearer {token}"));
        }
        headers
    }

    /// Fetch with auth headers. A 401 (or a 403 asking for more scope) goes to the auth provider
    /// once, and the request is retried with whatever credentials it left behind.
    async fn authorized_fetch(
        &self,
        method: Method,
        extra: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<Response, McpError> {
        let provider = self.options.auth_provider.clone();
        for attempt in 0.. {
            let token = match &provider {
                Some(provider) => provider.token().await?,
                None => None,
            };
            self.lock().last_token = token.clone();
            let mut request = self
                .client
                .request(method.clone(), self.url.clone())
                .headers(self.build_headers(extra, token.as_deref()));
            if let Some(body) = body {
                request = request.body(body.to_string());
            }
            let response = tokio::select! {
                response = request.send() => response.map_err(McpError::from_reqwest)?,
                _ = self.cancel.cancelled() => return Err(McpError::closed()),
            };
            let status = response.status().as_u16();
            let Some(provider) =
                provider.as_ref().filter(|_| attempt == 0 && needs_authorization(status, response.headers()))
            else {
                return Ok(response);
            };
            let context = UnauthorizedContext {
                status,
                headers: response.headers().clone(),
                server_url: self.url.clone(),
                client: self.client.clone(),
                token,
            };
            if !provider.on_unauthorized(context).await? {
                return Ok(response);
            }
        }
        unreachable!("the loop returns")
    }

    async fn check_response(&self, mut response: Response) -> Result<Response, McpError> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let www_authenticate = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        // Spare bytes finish a character that the read limit cuts, so the truncation below keeps
        // whole characters.
        let (bytes, _) = read_body_limited(&mut response, MAX_ERROR_BODY_BYTES + 3).await.unwrap_or_default();
        let mut body = String::from_utf8_lossy(&bytes).into_owned();
        if body.len() > MAX_ERROR_BODY_BYTES {
            let mut end = MAX_ERROR_BODY_BYTES;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
        }
        if status == 401 {
            return Err(McpError::AuthRequired { body, www_authenticate });
        }
        if status == 404 && self.lock().session_id.is_some() {
            return Err(McpError::SessionExpired { body });
        }
        Err(McpError::Http { status, message: describe_http_failure(status, &body), body })
    }

    fn capture_session(&self, response: &Response) {
        let session = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok());
        if let Some(session) = session.filter(|session| !session.is_empty()) {
            self.lock().session_id = Some(session.to_string());
        }
    }

    async fn send(self: &Arc<Self>, message: &JsonRpcMessage) -> Result<(), McpError> {
        {
            let state = self.lock();
            if !state.started || state.closed {
                return Err(McpError::closed());
            }
        }
        let headers = [
            ("accept", "application/json, text/event-stream".to_string()),
            ("content-type", "application/json".to_string()),
        ];
        let response = self.authorized_fetch(Method::POST, &headers, Some(&message.to_json())).await?;
        let mut response = self.check_response(response).await?;
        self.capture_session(&response);
        let JsonRpcMessage::Request { id, method, .. } = message else {
            // Notifications and responses are acknowledged with 202 and carry no reply.
            drop(response);
            if message.method() == Some("notifications/initialized") {
                self.start_get_stream();
            }
            return Ok(());
        };
        let status = response.status().as_u16();
        if status == 202 || status == 204 {
            let message = format!("MCP server accepted request {method} without a response");
            return Err(McpError::Http { status, message, body: String::new() });
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let max = self.options.max_message_bytes;
                let (bytes, exceeded) = read_body_limited(&mut response, max).await?;
                if exceeded {
                    return Err(McpError::other(format!("MCP JSON response exceeds {max} bytes")));
                }
                let body: Value = serde_json::from_slice(&bytes)
                    .map_err(|error| McpError::Network(format!("error decoding response body: {error}")))?;
                let items = match body {
                    Value::Array(items) => items,
                    body => vec![body],
                };
                let messages = items.iter().map(JsonRpcMessage::parse).collect::<Result<Vec<_>, _>>()?;
                if let Some(sink) = self.sink() {
                    for message in messages {
                        sink.message(message);
                    }
                }
                Ok(())
            }
            Some("text/event-stream") => {
                tokio::spawn(self.clone().consume_response_stream(response, id.clone()));
                Ok(())
            }
            other => Err(McpError::Http {
                status,
                message: format!("Unsupported MCP response content type: {}", other.unwrap_or("missing")),
                body: String::new(),
            }),
        }
    }

    async fn close(&self) {
        let (started, session, token, sink) = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            (state.started, state.session_id.clone(), state.last_token.clone(), state.sink.clone())
        };
        self.cancel.cancel();
        if started && session.is_some() {
            // Best effort: the session expires on the server anyway. Closing never waits for a refresh.
            let request = self
                .client
                .delete(self.url.clone())
                .headers(self.build_headers(&[], token.as_deref()))
                .timeout(Duration::from_secs(1));
            let _ = request.send().await;
        }
        if let Some(sink) = sink {
            sink.close();
        }
    }
}

#[async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn start(&self, sink: TransportSink) -> Result<(), McpError> {
        let mut state = self.inner.lock();
        if state.started {
            return Err(McpError::other("MCP Streamable HTTP transport already started"));
        }
        if state.closed {
            return Err(McpError::closed());
        }
        state.started = true;
        state.sink = Some(sink);
        Ok(())
    }

    async fn send(&self, message: &JsonRpcMessage) -> Result<(), McpError> {
        self.inner.send(message).await
    }

    async fn close(&self) {
        self.inner.close().await;
    }

    fn set_protocol_version(&self, version: &str) {
        self.inner.lock().protocol_version = Some(version.to_string());
    }
}
