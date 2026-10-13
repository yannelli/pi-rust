//! Routing of incoming messages, request waiting, and connection teardown.

use std::sync::{Arc, MutexGuard, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Notify, oneshot};
use tokio_util::sync::CancellationToken;

use super::{CloseListener, ErrorListener, Inner, Listeners, NotificationListener, Shared};
use crate::client::ClientState;
use crate::error::{McpError, codes};
use crate::jsonrpc::{JsonRpcErrorObject, JsonRpcId, JsonRpcMessage};
use crate::transport::{SharedTransport, TransportEvent};
use crate::types::ProgressNotification;

pub(super) async fn run(
    inner: Weak<Inner>,
    generation: u64,
    mut events: tokio::sync::mpsc::UnboundedReceiver<TransportEvent>,
) {
    while let Some(event) = events.recv().await {
        let Some(inner) = inner.upgrade() else { return };
        if !inner.handle_event(generation, event) {
            return;
        }
    }
}

pub(super) enum Outcome {
    Response(Result<Value, McpError>),
    SendFailed(McpError),
    TimedOut,
    Aborted,
}

/// Removes the pending entry when the caller drops the request future.
pub(super) struct PendingGuard<'a> {
    inner: &'a Arc<Inner>,
    id: JsonRpcId,
    armed: bool,
}

impl<'a> PendingGuard<'a> {
    pub(super) fn new(inner: &'a Arc<Inner>, id: JsonRpcId) -> Self {
        Self { inner, id, armed: true }
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.shared().pending.remove(&self.id);
        }
    }
}

/// Send the request and wait for its response, a timeout (restarted by progress), or cancellation.
pub(super) async fn wait_for_response(
    transport: &SharedTransport,
    message: &JsonRpcMessage,
    mut receiver: oneshot::Receiver<Result<Value, McpError>>,
    notify: &Notify,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Outcome {
    let send = transport.send(message);
    tokio::pin!(send);
    let mut sent = false;
    let limit = if timeout.is_zero() { Duration::from_secs(u32::MAX as u64) } else { timeout };
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            biased;
            response = &mut receiver => {
                return Outcome::Response(response.unwrap_or_else(|_| Err(McpError::closed())));
            }
            result = &mut send, if !sent => {
                sent = true;
                if let Err(error) = result {
                    return Outcome::SendFailed(error);
                }
            }
            _ = cancel.cancelled() => return Outcome::Aborted,
            _ = notify.notified() => deadline.as_mut().reset(tokio::time::Instant::now() + limit),
            _ = &mut deadline => return Outcome::TimedOut,
        }
    }
}

impl Inner {
    pub(super) fn shared(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn listeners(&self) -> MutexGuard<'_, Listeners> {
        self.listeners.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit_error(&self, error: &McpError) {
        let listeners: Vec<ErrorListener> =
            self.listeners().errors.iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(error);
        }
    }

    /// Fail a pending request, telling the server with `notifications/cancelled` when `reason` is set.
    pub(super) fn cancel_pending(&self, id: &JsonRpcId, error: McpError, reason: Option<&str>) {
        let (entry, transport) = {
            let mut shared = self.shared();
            (shared.pending.remove(id), shared.transport.clone())
        };
        let Some(entry) = entry else { return };
        let _ = entry.sender.send(Err(error));
        if let (Some(reason), Some(transport)) = (reason, transport) {
            let message = JsonRpcMessage::notification(
                "notifications/cancelled",
                Some(json!({ "requestId": id.to_value(), "reason": reason })),
            );
            tokio::spawn(async move {
                let _ = transport.send(&message).await;
            });
        }
    }

    /// Idempotent: fails in-flight requests, aborts requests being served, and flips the state.
    pub(super) fn mark_closed(&self, error: McpError) {
        let (was_closed, pending, incoming) = {
            let mut shared = self.shared();
            let was_closed = shared.state == ClientState::Closed;
            shared.state = ClientState::Closed;
            (was_closed, std::mem::take(&mut shared.pending), std::mem::take(&mut shared.incoming))
        };
        for (_, entry) in pending {
            let _ = entry.sender.send(Err(error.clone()));
        }
        for (_, token) in incoming {
            token.cancel();
        }
        if was_closed {
            return;
        }
        let listeners: Vec<CloseListener> =
            self.listeners().closes.iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener();
        }
    }

    /// Returns false once the event stream should stop.
    fn handle_event(self: &Arc<Self>, generation: u64, event: TransportEvent) -> bool {
        if self.shared().generation != generation {
            return false;
        }
        match event {
            TransportEvent::Message(message) => self.handle_message(message),
            TransportEvent::Error(error) => self.emit_error(&error),
            TransportEvent::Closed => {
                self.mark_closed(McpError::closed());
                return false;
            }
        }
        true
    }

    fn handle_message(self: &Arc<Self>, message: JsonRpcMessage) {
        match message {
            JsonRpcMessage::Response { id, result } => {
                let entry = self.shared().pending.remove(&id);
                match entry {
                    Some(entry) => {
                        let _ = entry.sender.send(result.map_err(McpError::from));
                    }
                    None => {
                        self.emit_error(&McpError::other(format!("Received response for unknown MCP request {id}")))
                    }
                }
            }
            JsonRpcMessage::Request { id, method, params } => {
                tokio::spawn(handle_request(self.clone(), id, method, params));
            }
            JsonRpcMessage::Notification { method, params } => self.handle_notification(&method, params.as_ref()),
        }
    }

    fn handle_notification(&self, method: &str, params: Option<&Value>) {
        match method {
            "notifications/progress" => self.handle_progress(params),
            "notifications/cancelled" => {
                let id = params
                    .and_then(|params| serde_json::from_value::<JsonRpcId>(params.get("requestId")?.clone()).ok());
                if let Some(token) = id.and_then(|id| self.shared().incoming.get(&id).cloned()) {
                    token.cancel();
                }
            }
            _ => {}
        }
        let listeners: Vec<NotificationListener> = self
            .listeners()
            .notifications
            .get(method)
            .map(|entries| entries.iter().map(|(_, listener)| listener.clone()).collect())
            .unwrap_or_default();
        for listener in listeners {
            listener(params);
        }
    }

    fn handle_progress(&self, params: Option<&Value>) {
        let Some(progress) =
            params.and_then(|params| serde_json::from_value::<ProgressNotification>(params.clone()).ok())
        else {
            return;
        };
        let entry = self.shared().pending.get(&progress.progress_token).and_then(|entry| entry.progress.clone());
        if let Some((notify, on_progress)) = entry {
            notify.notify_one();
            on_progress(&progress);
        }
    }
}

async fn handle_request(inner: Arc<Inner>, id: JsonRpcId, method: String, params: Option<Value>) {
    let (transport, handler) = {
        let shared = inner.shared();
        (shared.transport.clone(), shared.handlers.get(&method).cloned())
    };
    let Some(transport) = transport else { return };
    let response = match handler {
        None => JsonRpcMessage::error(id.clone(), codes::METHOD_NOT_FOUND, format!("Method not found: {method}")),
        Some(handler) => {
            let token = CancellationToken::new();
            inner.shared().incoming.insert(id.clone(), token.clone());
            let result = handler(params, token).await;
            inner.shared().incoming.remove(&id);
            match result {
                Ok(Value::Null) => JsonRpcMessage::result(id.clone(), json!({})),
                Ok(value) => JsonRpcMessage::result(id.clone(), value),
                Err(McpError::Rpc { code, message, data }) => {
                    JsonRpcMessage::Response { id: id.clone(), result: Err(JsonRpcErrorObject { code, message, data }) }
                }
                Err(error) => JsonRpcMessage::error(id.clone(), codes::INTERNAL_ERROR, error.to_string()),
            }
        }
    };
    if let Err(error) = transport.send(&response).await {
        inner.emit_error(&error);
    }
}
