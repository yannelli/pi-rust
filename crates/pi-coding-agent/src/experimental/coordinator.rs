//! The server-side endpoint of the coordinator's opaque message router (upstream `experimental/coordinator.ts`).
//! The coordinator process itself is in `coordinator_process`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::{ChordError, ChordResult};
use serde_json::{Value, json};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use super::control::{LineReader, LineWriter, try_connect};
use super::process::{InternalProcessRole, InternalProcessSpawnOptions, lock, spawn_internal_process};

pub const COORDINATOR_PROTOCOL_VERSION: u64 = 3;
const COORDINATOR_START_TIMEOUT: Duration = Duration::from_secs(10);
const COORDINATOR_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, PartialEq)]
pub enum CoordinatorConnectionEvent {
    PeerConnected { peer_id: String },
    PeerDisconnected { peer_id: String },
    Message { from: String, payload: Value },
}

pub type CoordinatorEventListener = Arc<dyn Fn(&CoordinatorConnectionEvent) + Send + Sync>;
pub type RemoveListener = Box<dyn FnOnce() + Send>;

/// What the Session worker manager needs from a coordinator connection.
#[async_trait]
pub trait WorkerCoordinator: Send + Sync {
    fn control_path(&self) -> PathBuf;
    fn server_connection_id(&self) -> String;
    fn was_replaced(&self) -> bool;
    fn on_event(&self, listener: CoordinatorEventListener) -> RemoveListener;
    async fn send(&self, peer_id: &str, payload: Value) -> ChordResult<()>;
    async fn broadcast(&self, payload: Value) -> ChordResult<()>;
}

#[derive(Default)]
struct ConnectionState {
    writer: Option<LineWriter>,
    reader: Option<JoinHandle<()>>,
    registered: bool,
    closed: bool,
    pending_registration: Option<oneshot::Sender<ChordResult<()>>>,
}

pub struct CoordinatorConnection {
    server_connection_id: String,
    control_path: PathBuf,
    endpoint: String,
    peer_ids: Mutex<HashSet<String>>,
    listeners: Mutex<BTreeMap<u64, CoordinatorEventListener>>,
    next_listener: AtomicU64,
    state: Mutex<ConnectionState>,
    replaced: watch::Sender<bool>,
}

impl CoordinatorConnection {
    pub fn new(control_path: PathBuf, endpoint: String, server_connection_id: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            server_connection_id: server_connection_id.unwrap_or_else(pi_server::random_uuid),
            control_path,
            endpoint,
            peer_ids: Mutex::default(),
            listeners: Mutex::default(),
            next_listener: AtomicU64::new(0),
            state: Mutex::default(),
            replaced: watch::channel(false).0,
        })
    }

    pub fn peer_ids(&self) -> HashSet<String> {
        lock(&self.peer_ids).clone()
    }

    /// Resolves once another server generation replaced this one or the connection was lost.
    pub async fn replaced(&self) {
        let mut replaced = self.replaced.subscribe();
        let _ = replaced.wait_for(|value| *value).await;
    }

    pub async fn connect(self: &Arc<Self>) -> ChordResult<()> {
        if lock(&self.state).writer.is_some() {
            return Err(ChordError::error("Coordinator server is already connected"));
        }
        let stream = UnixStream::connect(&self.control_path).await.map_err(ChordError::other)?;
        let (read, write) = stream.into_split();
        let writer = LineWriter::new(write);
        let (registered, wait) = oneshot::channel();
        let this = Arc::downgrade(self);
        let reader = tokio::spawn(async move {
            let mut reader = LineReader::new(read);
            let error = loop {
                let message = match reader.next("Coordinator").await {
                    Ok(Some(message)) => message,
                    Ok(None) => break ChordError::error("Coordinator connection closed"),
                    Err(error) => break error,
                };
                let Some(this) = this.upgrade() else { return };
                if let Err(error) = this.handle_message(message) {
                    break error;
                }
            };
            if let Some(this) = this.upgrade() {
                this.disconnected(error);
            }
        });
        {
            let mut state = lock(&self.state);
            state.writer = Some(writer.clone());
            state.reader = Some(reader);
            state.pending_registration = Some(registered);
        }
        let register = json!({
            "type": "register_server",
            "protocol": COORDINATOR_PROTOCOL_VERSION,
            "serverConnectionId": self.server_connection_id,
            "endpoint": self.endpoint,
        });
        writer.write(&register).await?;
        wait.await.map_err(|_| ChordError::error("Coordinator server closed"))?
    }

    pub fn close(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        state.closed = true;
        state.writer = None;
        if let Some(reader) = state.reader.take() {
            reader.abort();
        }
        if let Some(pending) = state.pending_registration.take() {
            let _ = pending.send(Err(ChordError::error("Coordinator server closed")));
        }
        drop(state);
        lock(&self.peer_ids).clear();
    }

    async fn write(&self, message: Value) -> ChordResult<()> {
        let writer = {
            let state = lock(&self.state);
            match (&state.writer, state.registered && !state.closed) {
                (Some(writer), true) => writer.clone(),
                _ => return Err(ChordError::error("Coordinator server is not connected")),
            }
        };
        writer.write(&message).await
    }

    fn handle_message(&self, value: Value) -> ChordResult<()> {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or_default();
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        let invalid = || ChordError::error("Coordinator sent an invalid message");
        match kind {
            "server_registered" => {
                let id = text("serverConnectionId").ok_or_else(invalid)?;
                let peers = value.get("peers").and_then(Value::as_array).ok_or_else(invalid)?;
                let peers: Option<Vec<String>> = peers.iter().map(|peer| peer.as_str().map(str::to_string)).collect();
                let peers = peers.ok_or_else(invalid)?;
                if id != self.server_connection_id {
                    return Err(ChordError::error("Coordinator returned an invalid server registration"));
                }
                lock(&self.peer_ids).extend(peers);
                let mut state = lock(&self.state);
                state.registered = true;
                if let Some(pending) = state.pending_registration.take() {
                    let _ = pending.send(Ok(()));
                }
            }
            "server_replaced" => self.mark_replaced(),
            "peer_connected" => {
                let peer_id = text("peerId").ok_or_else(invalid)?;
                lock(&self.peer_ids).insert(peer_id.clone());
                self.emit(&CoordinatorConnectionEvent::PeerConnected { peer_id });
            }
            "peer_disconnected" => {
                let peer_id = text("peerId").ok_or_else(invalid)?;
                lock(&self.peer_ids).remove(&peer_id);
                self.emit(&CoordinatorConnectionEvent::PeerDisconnected { peer_id });
            }
            "message" => {
                let from = text("from").ok_or_else(invalid)?;
                let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                self.emit(&CoordinatorConnectionEvent::Message { from, payload });
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }

    fn emit(&self, event: &CoordinatorConnectionEvent) {
        let listeners: Vec<CoordinatorEventListener> = lock(&self.listeners).values().cloned().collect();
        for listener in listeners {
            listener(event);
        }
    }

    fn disconnected(&self, error: ChordError) {
        let mut state = lock(&self.state);
        state.writer = None;
        if state.closed {
            return;
        }
        if let Some(pending) = state.pending_registration.take() {
            let _ = pending.send(Err(error));
        }
        drop(state);
        self.mark_replaced();
    }

    fn mark_replaced(&self) {
        self.replaced.send_if_modified(|value| !std::mem::replace(value, true));
    }
}

#[async_trait]
impl WorkerCoordinator for Arc<CoordinatorConnection> {
    fn control_path(&self) -> PathBuf {
        self.control_path.clone()
    }

    fn server_connection_id(&self) -> String {
        self.server_connection_id.clone()
    }

    fn was_replaced(&self) -> bool {
        *self.replaced.borrow()
    }

    fn on_event(&self, listener: CoordinatorEventListener) -> RemoveListener {
        let id = self.next_listener.fetch_add(1, Ordering::SeqCst);
        lock(&self.listeners).insert(id, listener);
        let weak = Arc::downgrade(self);
        Box::new(move || {
            if let Some(this) = weak.upgrade() {
                lock(&this.listeners).remove(&id);
            }
        })
    }

    async fn send(&self, peer_id: &str, payload: Value) -> ChordResult<()> {
        self.write(json!({ "type": "send", "to": peer_id, "payload": payload })).await
    }

    async fn broadcast(&self, payload: Value) -> ChordResult<()> {
        self.write(json!({ "type": "broadcast", "payload": payload })).await
    }
}

/// Holds a control connection open so the coordinator does not retire while the server starts.
pub struct CoordinatorStartupLease {
    _socket: UnixStream,
}

impl CoordinatorStartupLease {
    pub fn close(self) {}
}

/// Connect to the coordinator at `control_path`, launching one when none is running.
pub async fn ensure_coordinator(public_path: &Path, control_path: &Path) -> ChordResult<CoordinatorStartupLease> {
    if let Some(socket) = try_connect(control_path).await? {
        return Ok(CoordinatorStartupLease { _socket: socket });
    }
    let args = [public_path.display().to_string(), control_path.display().to_string()];
    let child =
        spawn_internal_process(InternalProcessRole::Coordinator, &args, InternalProcessSpawnOptions::default())?;
    let deadline = tokio::time::Instant::now() + COORDINATOR_START_TIMEOUT;
    loop {
        if let Some(socket) = try_connect(control_path).await? {
            return Ok(CoordinatorStartupLease { _socket: socket });
        }
        if child.has_exited() {
            return Err(ChordError::error("Coordinator exited during startup"));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(ChordError::error("Timed out waiting for coordinator startup"));
        }
        tokio::time::sleep(COORDINATOR_RETRY).await;
    }
}
