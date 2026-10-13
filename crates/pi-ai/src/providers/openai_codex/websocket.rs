//! Codex WebSocket transport: rustls connections, per-session cached sockets with
//! continuation state, and the debug statistics upstream exposes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::{FutureExt, StreamExt};
use reqwest::Url;
use serde_json::Value;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message as WsMessage};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream, client_async_tls_with_config};

use super::events::CodexError;
use super::request::HeaderList;
use super::tunnel::{self, Tunnel};

pub(super) type Socket = WebSocketStream<MaybeTlsStream<Tunnel>>;
type Key = (String, String);

/// Idle cached sockets close after this long.
const IDLE_TTL: Duration = Duration::from_secs(5 * 60);
/// The backend drops connections at 60 minutes; reconnect before that.
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
pub(super) const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;

/// Per-session counters, as upstream `getOpenAICodexWebSocketDebugStats` reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodexWebSocketDebugStats {
    pub requests: u64,
    pub connections_created: u64,
    pub connections_reused: u64,
    pub cached_context_requests: u64,
    pub store_true_requests: u64,
    pub full_context_requests: u64,
    pub delta_requests: u64,
    pub last_input_items: usize,
    pub last_delta_input_items: Option<usize>,
    pub last_previous_response_id: Option<String>,
    pub websocket_failures: u64,
    pub sse_fallbacks: u64,
    pub websocket_fallback_active: Option<bool>,
    pub last_websocket_error: Option<String>,
}

/// The last request on a cached socket, so the next one can send only new input.
#[derive(Clone, Debug)]
pub(super) struct Continuation {
    pub last_request_body: Value,
    pub last_response_id: String,
    pub last_response_items: Vec<Value>,
}

struct Idle {
    socket: Socket,
    created_at: Instant,
    continuation: Option<Continuation>,
    generation: u64,
}

#[derive(Default)]
struct State {
    idle: HashMap<Key, Idle>,
    busy: HashSet<Key>,
    generation: u64,
    stats: HashMap<String, CodexWebSocketDebugStats>,
    sse_fallback: HashSet<String>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Debug counters for a session, if it has sent WebSocket requests.
pub fn websocket_debug_stats(session_id: &str) -> Option<CodexWebSocketDebugStats> {
    state().stats.get(session_id).cloned()
}

/// Clear debug counters and SSE fallback state for one session or all of them.
pub fn reset_websocket_debug_stats(session_id: Option<&str>) {
    let mut state = state();
    match session_id {
        Some(session) => {
            state.stats.remove(session);
            state.sse_fallback.remove(session);
        }
        None => {
            state.stats.clear();
            state.sse_fallback.clear();
        }
    }
}

/// Close cached sockets for one session or all sessions.
pub fn close_websocket_sessions(session_id: Option<&str>) {
    let closing: Vec<Idle> = {
        let mut state = state();
        let keys: Vec<Key> =
            state.idle.keys().filter(|(session, _)| session_id.is_none_or(|id| id == session)).cloned().collect();
        keys.iter().filter_map(|key| state.idle.remove(key)).collect()
    };
    for idle in closing {
        close_later(idle.socket, "debug_close");
    }
}

pub(super) fn with_stats(session: Option<&str>, update: impl FnOnce(&mut CodexWebSocketDebugStats)) {
    if let Some(session) = session {
        update(state().stats.entry(session.to_string()).or_default());
    }
}

pub(super) fn sse_fallback_active(session: Option<&str>) -> bool {
    session.is_some_and(|session| state().sse_fallback.contains(session))
}

pub(super) fn record_sse_fallback(session: Option<&str>) {
    let active = sse_fallback_active(session);
    with_stats(session, |stats| {
        stats.sse_fallbacks += 1;
        stats.websocket_fallback_active = Some(active);
    });
}

/// After a WebSocket failure the session uses SSE until the stats are reset.
pub(super) fn record_failure(session: Option<&str>, error: &str) {
    let Some(session) = session else { return };
    state().sse_fallback.insert(session.to_string());
    with_stats(Some(session), |stats| {
        stats.websocket_failures += 1;
        stats.last_websocket_error = Some(error.to_string());
        stats.websocket_fallback_active = Some(true);
    });
}

/// Connect, through the applicable proxy, within `timeout_ms` for the whole handshake.
async fn connect(
    url: &str,
    headers: &HeaderList,
    env: &BTreeMap<String, String>,
    timeout_ms: u64,
) -> Result<Socket, CodexError> {
    let transport = |message: String| CodexError::Transport(message);
    let mut request =
        url.into_client_request().map_err(|error| transport(format!("Invalid WebSocket URL: {error}")))?;
    for (name, value) in &headers.0 {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            request.headers_mut().insert(name, value);
        }
    }
    let target = Url::parse(url).map_err(|error| transport(format!("Invalid WebSocket URL: {error}")))?;
    let connecting = async {
        let stream = tunnel::open(&target, env).await?;
        let connector = Connector::Rustls(tunnel::tls_config());
        client_async_tls_with_config(request, stream, None, Some(connector)).await.map_err(handshake_error)
    };
    let result = if timeout_ms > 0 {
        tokio::time::timeout(Duration::from_millis(timeout_ms), connecting)
            .await
            .map_err(|_| transport(format!("WebSocket connect timeout after {timeout_ms}ms")))?
    } else {
        connecting.await
    };
    result.map(|(socket, _)| socket)
}

fn handshake_error(error: WsError) -> CodexError {
    match error {
        WsError::Http(response) => {
            CodexError::Transport(format!("Unexpected server response: {}", response.status().as_u16()))
        }
        error => CodexError::Transport(error.to_string()),
    }
}

fn close_later(mut socket: Socket, reason: &'static str) {
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    tokio::spawn(async move {
        let frame = CloseFrame { code: CloseCode::Normal, reason: reason.into() };
        let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(Some(frame))).await;
    });
}

/// False when the peer already closed the socket or sent something unexpected while idle.
fn is_reusable(socket: &mut Socket) -> bool {
    matches!(socket.next().now_or_never(), None | Some(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))))
}

/// A socket checked out for one request.
pub(super) struct Lease {
    pub socket: Option<Socket>,
    key: Option<Key>,
    /// Whether the socket belongs to the session cache (it may carry continuation state).
    pub cached: bool,
    pub reused: bool,
    pub continuation: Option<Continuation>,
    created_at: Instant,
}

impl Lease {
    pub fn socket(&mut self) -> &mut Socket {
        self.socket.as_mut().expect("socket is present until release")
    }

    /// Return a cached socket for reuse when `keep`, otherwise close it.
    pub fn release(mut self, keep: bool, continuation: Option<Continuation>) {
        let Some(mut socket) = self.socket.take() else { return };
        let Some(key) = self.key.take() else { return close_later(socket, "done") };
        let reusable = keep && is_reusable(&mut socket);
        let mut guard = state();
        guard.busy.remove(&key);
        if !reusable {
            drop(guard);
            return close_later(socket, "done");
        }
        guard.generation += 1;
        let generation = guard.generation;
        guard.idle.insert(key.clone(), Idle { socket, created_at: self.created_at, continuation, generation });
        drop(guard);
        tokio::spawn(async move {
            tokio::time::sleep(IDLE_TTL).await;
            let expired = {
                let mut guard = state();
                let current = guard.idle.get(&key).is_some_and(|idle| idle.generation == generation);
                current.then(|| guard.idle.remove(&key)).flatten()
            };
            if let Some(idle) = expired {
                close_later(idle.socket, "idle_timeout");
            }
        });
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            state().busy.remove(&key);
        }
    }
}

/// A cached socket for `(session, account)` when one is idle and fresh, else a new one.
/// Without a session, or while the cached socket is busy, the socket is one-shot.
pub(super) async fn acquire(
    url: &str,
    headers: &HeaderList,
    session: Option<&str>,
    account: &str,
    env: &BTreeMap<String, String>,
    connect_timeout_ms: u64,
) -> Result<Lease, CodexError> {
    let one_shot = |socket| Lease {
        socket: Some(socket),
        key: None,
        cached: false,
        reused: false,
        continuation: None,
        created_at: Instant::now(),
    };
    let Some(session) = session else { return Ok(one_shot(connect(url, headers, env, connect_timeout_ms).await?)) };
    let key = (session.to_string(), account.to_string());
    let cached = {
        let mut state = state();
        if state.busy.contains(&key) {
            None
        } else {
            state.busy.insert(key.clone());
            Some(state.idle.remove(&key))
        }
    };
    let Some(cached) = cached else { return Ok(one_shot(connect(url, headers, env, connect_timeout_ms).await?)) };
    let mut lease = Lease {
        socket: None,
        key: Some(key),
        cached: true,
        reused: false,
        continuation: None,
        created_at: Instant::now(),
    };
    if let Some(mut idle) = cached {
        if idle.created_at.elapsed() < MAX_AGE && is_reusable(&mut idle.socket) {
            lease.socket = Some(idle.socket);
            lease.reused = true;
            lease.continuation = idle.continuation;
            lease.created_at = idle.created_at;
            return Ok(lease);
        }
        let reason = if idle.created_at.elapsed() >= MAX_AGE { "connection_age_limit" } else { "done" };
        close_later(idle.socket, reason);
    }
    lease.socket = Some(connect(url, headers, env, connect_timeout_ms).await?);
    Ok(lease)
}

/// Make the idle socket for a session look older, to exercise the age limit in tests.
#[cfg(test)]
pub(super) fn age_cached_socket(session: &str, by: Duration) {
    for ((cached_session, _), idle) in state().idle.iter_mut() {
        if cached_session == session {
            idle.created_at = idle.created_at.checked_sub(by).unwrap_or(idle.created_at);
        }
    }
}
