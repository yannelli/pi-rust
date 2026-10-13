//! SSE streams of the HTTP transport: response streams and the server-to-client GET stream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use reqwest::{Method, Response};

use super::{Inner, content_type};
use crate::error::{McpError, codes};
use crate::jsonrpc::{JsonRpcId, JsonRpcMessage};
use crate::transport::sse::{SseItem, SseParser};

#[derive(Default)]
struct StreamCursor {
    last_event_id: Option<String>,
    retry: Option<Duration>,
    /// Whether the stream delivered an event since it was opened.
    received: bool,
}

/// Network failures and transient statuses are retried; auth, session, and protocol errors are not.
fn is_retryable(error: &McpError) -> bool {
    match error {
        McpError::Http { status, .. } => *status == 408 || *status == 429 || *status >= 500,
        McpError::Network(_) => true,
        _ => false,
    }
}

impl Inner {
    fn reconnect_delay(&self, attempt: u32, server_delay: Option<Duration>) -> Duration {
        if let Some(delay) = server_delay {
            return delay;
        }
        let reconnect = self.options.reconnect;
        reconnect.initial_delay.saturating_mul(2u32.saturating_pow(attempt)).min(reconnect.max_delay)
    }

    /// Returns false when the transport closed while waiting.
    async fn sleep(&self, delay: Duration) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => !self.cancel.is_cancelled(),
            _ = self.cancel.cancelled() => false,
        }
    }

    async fn consume_sse(
        &self,
        response: Response,
        cursor: &mut StreamCursor,
        mut on_message: impl FnMut(&JsonRpcMessage),
    ) -> Result<(), McpError> {
        let mut parser = SseParser::new(self.options.max_message_bytes);
        let mut body = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                chunk = body.next() => chunk,
                _ = self.cancel.cancelled() => return Ok(()),
            };
            let (items, ended) = match chunk {
                None => (parser.finish()?, true),
                Some(Err(error)) => return Err(McpError::from_reqwest(error)),
                Some(Ok(bytes)) => (parser.push(&bytes)?, false),
            };
            for item in items {
                self.handle_item(item, cursor, &mut on_message);
            }
            if ended {
                return Ok(());
            }
        }
    }

    fn handle_item(&self, item: SseItem, cursor: &mut StreamCursor, on_message: &mut impl FnMut(&JsonRpcMessage)) {
        let event = match item {
            SseItem::Id(id) => return cursor.last_event_id = Some(id),
            SseItem::Retry(delay) => return cursor.retry = Some(Duration::from_millis(delay)),
            SseItem::Event(event) => event,
        };
        cursor.received = true;
        // Events without data prime resumption; other event types are not JSON-RPC.
        if event.data.trim().is_empty() || event.event.as_deref().is_some_and(|name| name != "message") {
            return;
        }
        let Some(sink) = self.sink() else { return };
        match JsonRpcMessage::from_json(&event.data) {
            Ok(message) => {
                on_message(&message);
                sink.message(message);
            }
            Err(error) => sink.error(error),
        }
    }

    /// Read the SSE stream answering one request. When it ends before the response and the server
    /// assigned event IDs, resume it with GET and `Last-Event-ID`. Otherwise only this request fails.
    pub(super) async fn consume_response_stream(self: Arc<Self>, response: Response, request_id: JsonRpcId) {
        let mut cursor = StreamCursor::default();
        let mut answered = false;
        let mut stream = Some(response);
        let mut failure: Option<McpError> = None;
        let mut attempt = 0;
        loop {
            if let Some(response) = stream.take() {
                let result = self
                    .consume_sse(response, &mut cursor, |message| {
                        if matches!(message, JsonRpcMessage::Response { id, .. } if *id == request_id) {
                            answered = true;
                        }
                    })
                    .await;
                failure = result.err();
            }
            if answered || self.closed() {
                return;
            }
            if failure.as_ref().is_some_and(|failure| !is_retryable(failure)) {
                break;
            }
            if cursor.last_event_id.is_none() || attempt >= self.options.reconnect.max_retries {
                break;
            }
            if cursor.received {
                attempt = 0;
            }
            cursor.received = false;
            let delay = self.reconnect_delay(attempt, cursor.retry);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
            match self.open_sse_stream(cursor.last_event_id.clone()).await {
                Ok(response) => stream = response,
                Err(error) => {
                    let retryable = is_retryable(&error);
                    failure = Some(error);
                    if !retryable {
                        break;
                    }
                }
            }
        }
        if self.closed() {
            return;
        }
        let reason =
            failure.map(|failure| failure.to_string()).unwrap_or_else(|| "stream ended without a response".into());
        if let Some(sink) = self.sink() {
            let message = format!("MCP response stream failed: {reason}");
            sink.message(JsonRpcMessage::error(request_id, codes::INTERNAL_ERROR, message));
        }
    }

    pub(super) fn start_get_stream(self: &Arc<Self>) {
        {
            let mut state = self.lock();
            if !self.options.open_get_stream || state.get_stream_started || state.closed {
                return;
            }
            state.get_stream_started = true;
        }
        tokio::spawn(self.clone().run_get_stream());
    }

    /// Keep the server-to-client stream open, reconnecting with backoff when it drops.
    async fn run_get_stream(self: Arc<Self>) {
        let mut cursor = StreamCursor::default();
        let mut attempt = 0;
        let report = |error: McpError| {
            if let Some(sink) = self.sink() {
                sink.error(error);
            }
        };
        while !self.closed() {
            let result = match self.open_sse_stream(cursor.last_event_id.clone()).await {
                // The server does not offer a GET stream.
                Ok(None) => return,
                Ok(Some(response)) => {
                    let opened = Instant::now();
                    let result = self.consume_sse(response, &mut cursor, |_| {}).await;
                    // A stream that stayed up for a while counts as healthy, even if it was idle.
                    if result.is_ok() && (cursor.received || opened.elapsed() > self.options.reconnect.max_delay) {
                        attempt = 0;
                    }
                    result
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                if self.closed() {
                    return;
                }
                if !is_retryable(&error) {
                    report(error);
                    return;
                }
            }
            cursor.received = false;
            if attempt >= self.options.reconnect.max_retries {
                report(McpError::other("MCP server-to-client stream dropped and could not be reopened"));
                return;
            }
            let delay = self.reconnect_delay(attempt, cursor.retry);
            attempt += 1;
            if !self.sleep(delay).await {
                return;
            }
        }
    }

    /// Open a GET SSE stream. `None` when the server answers 405 (no GET stream).
    async fn open_sse_stream(&self, last_event_id: Option<String>) -> Result<Option<Response>, McpError> {
        let mut headers = vec![("accept", "text/event-stream".to_string())];
        if let Some(id) = last_event_id {
            headers.push(("last-event-id", id));
        }
        let response = self.authorized_fetch(Method::GET, &headers, None).await?;
        if response.status().as_u16() == 405 {
            return Ok(None);
        }
        let response = self.check_response(response).await?;
        self.capture_session(&response);
        let kind = content_type(&response);
        if kind.as_deref() != Some("text/event-stream") {
            return Err(McpError::Http {
                status: response.status().as_u16(),
                message: format!("Unsupported MCP GET response content type: {}", kind.as_deref().unwrap_or("missing")),
                body: String::new(),
            });
        }
        Ok(Some(response))
    }
}
