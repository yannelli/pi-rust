//! The coordinator process (upstream `coordinator.ts` and `coordinator-entry.ts`). It is a transport shim: it
//! routes control lines and proxies public connections, and never interprets Pi payloads.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{ChordError, ChordResult};
use serde_json::{Value, json};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::control::{LineReader, LineWriter, signals};
use super::coordinator::COORDINATOR_PROTOCOL_VERSION;
use super::process::lock;

const EMPTY_STARTUP_GRACE: Duration = Duration::from_secs(30);
const EMPTY_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct ServerPeer {
    connection: u64,
    server_connection_id: String,
    endpoint: PathBuf,
    writer: LineWriter,
}

#[derive(Clone)]
struct RoutedPeer {
    connection: u64,
    writer: LineWriter,
}

#[derive(Default)]
struct State {
    peers: HashMap<String, RoutedPeer>,
    control_connections: HashMap<u64, JoinHandle<()>>,
    public_connections: HashMap<u64, JoinHandle<()>>,
    current_server: Option<ServerPeer>,
    shutting_down: bool,
    empty_timer: Option<JoinHandle<()>>,
}

struct Coordinator {
    state: Mutex<State>,
    next_id: AtomicU64,
    shutdown: Notify,
}

enum Role {
    Server(ServerPeer),
    Peer(String),
}

/// Run the coordinator until it has been empty for its grace period, then exit the process.
pub async fn run_coordinator_process(args: &[String]) -> ChordResult<()> {
    let (Some(public_path), Some(control_path)) = (args.first(), args.get(1)) else {
        return Err(ChordError::error("Coordinator requires public and control socket paths"));
    };
    let (public_path, control_path) = (PathBuf::from(public_path), PathBuf::from(control_path));
    remove_stale_socket(&control_path).await?;
    remove_stale_socket(&public_path).await?;
    let listeners = async {
        let control = UnixListener::bind(&control_path).map_err(ChordError::other)?;
        restrict_socket(&control_path)?;
        let public = UnixListener::bind(&public_path).map_err(ChordError::other)?;
        restrict_socket(&public_path)?;
        Ok::<_, ChordError>((control, public))
    };
    let (control, public) = match listeners.await {
        Ok(listeners) => listeners,
        Err(error) => {
            cleanup_socket(&control_path);
            cleanup_socket(&public_path);
            return Err(error);
        }
    };
    let coordinator =
        Arc::new(Coordinator { state: Mutex::default(), next_id: AtomicU64::new(1), shutdown: Notify::new() });
    coordinator.schedule_empty_shutdown(EMPTY_STARTUP_GRACE);
    let mut terminate = signals()?;
    loop {
        tokio::select! {
            accepted = control.accept() => if let Ok((stream, _)) = accepted { coordinator.accept_control(stream) },
            accepted = public.accept() => if let Ok((stream, _)) = accepted { coordinator.accept_public(stream) },
            () = coordinator.shutdown.notified() => break,
            () = terminate.recv() => break,
        }
    }
    coordinator.shut_down();
    cleanup_socket(&public_path);
    cleanup_socket(&control_path);
    std::process::exit(0);
}

impl Coordinator {
    fn accept_control(self: &Arc<Self>, stream: UnixStream) {
        if lock(&self.state).shutting_down {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (read, write) = stream.into_split();
        let writer = LineWriter::new(write);
        let this = self.clone();
        let mut state = lock(&self.state);
        let task = tokio::spawn(async move {
            let mut reader = LineReader::new(read);
            let mut role = None;
            while let Ok(Some(message)) = reader.next("Coordinator").await {
                if this.handle_control(id, &writer, &mut role, message).is_err() {
                    break;
                }
            }
            this.disconnect_control(id, role);
        });
        state.control_connections.insert(id, task);
        drop(state);
        self.cancel_empty_shutdown();
    }

    fn handle_control(&self, id: u64, writer: &LineWriter, role: &mut Option<Role>, message: Value) -> ChordResult<()> {
        let kind = message.get("type").and_then(Value::as_str);
        let Some(kind) = kind else { return Err(ChordError::error("Coordinator message must have a type")) };
        match role {
            None if kind == "register_server" => {
                *role = Some(Role::Server(self.register_server(id, writer, &message)?))
            }
            None if kind == "register_peer" => *role = Some(Role::Peer(self.register_peer(id, writer, &message)?)),
            None => return Err(ChordError::error("Coordinator connection did not register a role")),
            Some(Role::Server(_)) => {
                let current = lock(&self.state).current_server.as_ref().is_some_and(|current| current.connection == id);
                if current {
                    self.route("server", kind, &message)?;
                }
            }
            Some(Role::Peer(peer_id)) => self.route(peer_id, kind, &message)?,
        }
        Ok(())
    }

    fn register_server(&self, id: u64, writer: &LineWriter, message: &Value) -> ChordResult<ServerPeer> {
        if message.get("protocol").and_then(Value::as_u64) != Some(COORDINATOR_PROTOCOL_VERSION) {
            return Err(ChordError::error("Unsupported coordinator protocol"));
        }
        let text = |key: &str| message.get(key).and_then(Value::as_str).filter(|value| !value.is_empty());
        let server_connection_id = text("serverConnectionId")
            .ok_or_else(|| ChordError::error("Coordinator serverConnectionId must be a string"))?
            .to_string();
        let endpoint = text("endpoint").ok_or_else(|| ChordError::error("Coordinator endpoint must be a string"))?;
        let server = ServerPeer {
            connection: id,
            server_connection_id: server_connection_id.clone(),
            endpoint: PathBuf::from(endpoint),
            writer: writer.clone(),
        };
        self.cancel_empty_shutdown();
        let mut state = lock(&self.state);
        let previous = state.current_server.replace(server.clone());
        let peers: Vec<String> = state.peers.keys().cloned().collect();
        writer
            .post(&json!({ "type": "server_registered", "serverConnectionId": server_connection_id, "peers": peers }));
        if let Some(previous) = previous.filter(|previous| previous.connection != id) {
            for (_, task) in state.public_connections.drain() {
                task.abort();
            }
            notify_peers(
                &state,
                &json!({ "type": "server_disconnected", "serverConnectionId": previous.server_connection_id }),
            );
            previous.writer.post(&json!({ "type": "server_replaced" }));
        }
        notify_peers(&state, &json!({ "type": "server_connected", "serverConnectionId": server_connection_id }));
        Ok(server)
    }

    fn register_peer(&self, id: u64, writer: &LineWriter, message: &Value) -> ChordResult<String> {
        if message.get("protocol").and_then(Value::as_u64) != Some(COORDINATOR_PROTOCOL_VERSION) {
            return Err(ChordError::error("Unsupported coordinator protocol"));
        }
        let peer_id = message.get("peerId").and_then(Value::as_str).filter(|value| !value.is_empty());
        let peer_id = peer_id.ok_or_else(|| ChordError::error("Coordinator peerId must be a string"))?.to_string();
        let mut state = lock(&self.state);
        if peer_id == "server" || state.peers.contains_key(&peer_id) {
            return Err(ChordError::error(format!("Coordinator peer is already connected: {peer_id}")));
        }
        state.peers.insert(peer_id.clone(), RoutedPeer { connection: id, writer: writer.clone() });
        let mut registered = json!({ "type": "peer_registered", "peerId": peer_id });
        if let Some(server) = &state.current_server {
            registered["serverConnectionId"] = Value::from(server.server_connection_id.clone());
        }
        writer.post(&registered);
        if let Some(server) = &state.current_server {
            server.writer.post(&json!({ "type": "peer_connected", "peerId": peer_id }));
        }
        drop(state);
        self.cancel_empty_shutdown();
        Ok(peer_id)
    }

    fn route(&self, from: &str, kind: &str, message: &Value) -> ChordResult<()> {
        let payload = message.get("payload").cloned().unwrap_or(Value::Null);
        let state = lock(&self.state);
        match kind {
            "send" => {
                let to = message.get("to").and_then(Value::as_str).filter(|value| !value.is_empty());
                let to = to.ok_or_else(|| ChordError::error("Coordinator message target must be a string"))?;
                let target = if to == "server" {
                    state.current_server.as_ref().map(|server| &server.writer)
                } else {
                    state.peers.get(to).map(|peer| &peer.writer)
                };
                if let Some(target) = target {
                    target.post(&json!({ "type": "message", "from": from, "payload": payload }));
                }
                Ok(())
            }
            "broadcast" => {
                if from != "server" {
                    return Err(ChordError::error("Only the current server may broadcast"));
                }
                notify_peers(&state, &json!({ "type": "message", "from": from, "payload": payload }));
                Ok(())
            }
            other => Err(ChordError::error(format!("Unknown coordinator routing message: {other}"))),
        }
    }

    fn disconnect_control(self: &Arc<Self>, id: u64, role: Option<Role>) {
        let mut state = lock(&self.state);
        state.control_connections.remove(&id);
        match role {
            Some(Role::Server(server))
                if state.current_server.as_ref().is_some_and(|current| current.connection == id) =>
            {
                state.current_server = None;
                notify_peers(
                    &state,
                    &json!({ "type": "server_disconnected", "serverConnectionId": server.server_connection_id }),
                );
            }
            Some(Role::Peer(peer_id)) if state.peers.get(&peer_id).is_some_and(|peer| peer.connection == id) => {
                state.peers.remove(&peer_id);
                if let Some(server) = &state.current_server {
                    server.writer.post(&json!({ "type": "peer_disconnected", "peerId": peer_id }));
                }
            }
            _ => {}
        }
        drop(state);
        self.check_empty();
    }

    fn accept_public(self: &Arc<Self>, mut client: UnixStream) {
        let endpoint = {
            let state = lock(&self.state);
            match (&state.current_server, state.shutting_down) {
                (Some(server), false) => server.endpoint.clone(),
                _ => return,
            }
        };
        self.cancel_empty_shutdown();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let this = self.clone();
        let mut state = lock(&self.state);
        let task = tokio::spawn(async move {
            if let Ok(mut upstream) = UnixStream::connect(&endpoint).await {
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            }
            lock(&this.state).public_connections.remove(&id);
            this.check_empty();
        });
        state.public_connections.insert(id, task);
    }

    fn check_empty(self: &Arc<Self>) {
        let empty = {
            let state = lock(&self.state);
            !state.shutting_down && is_empty(&state)
        };
        if empty {
            self.schedule_empty_shutdown(EMPTY_SHUTDOWN_GRACE);
        } else {
            self.cancel_empty_shutdown();
        }
    }

    fn schedule_empty_shutdown(self: &Arc<Self>, delay: Duration) {
        let mut state = lock(&self.state);
        if state.empty_timer.is_some() || state.shutting_down {
            return;
        }
        let weak = Arc::downgrade(self);
        state.empty_timer = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(this) = weak.upgrade() else { return };
            let empty = {
                let mut state = lock(&this.state);
                state.empty_timer = None;
                is_empty(&state)
            };
            if empty {
                this.shutdown.notify_one();
            }
        }));
    }

    fn cancel_empty_shutdown(&self) {
        if let Some(timer) = lock(&self.state).empty_timer.take() {
            timer.abort();
        }
    }

    fn shut_down(&self) {
        let mut state = lock(&self.state);
        state.shutting_down = true;
        if let Some(timer) = state.empty_timer.take() {
            timer.abort();
        }
        let tasks: Vec<JoinHandle<()>> = state.public_connections.drain().map(|(_, task)| task).collect();
        tasks.into_iter().chain(state.control_connections.drain().map(|(_, task)| task)).for_each(|task| task.abort());
    }
}

fn is_empty(state: &State) -> bool {
    state.current_server.is_none()
        && state.peers.is_empty()
        && state.public_connections.is_empty()
        && state.control_connections.is_empty()
}

fn notify_peers(state: &State, message: &Value) {
    for peer in state.peers.values() {
        peer.writer.post(message);
    }
}

fn restrict_socket(path: &Path) -> ChordResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(ChordError::other)
}

async fn remove_stale_socket(path: &Path) -> ChordResult<()> {
    use std::os::unix::fs::FileTypeExt;
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return Err(ChordError::error(format!("Coordinator path is not a socket: {}", path.display())));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ChordError::other(error)),
    }
    if super::control::try_connect(path).await?.is_some() {
        return Err(ChordError::error(format!("Coordinator socket is already active: {}", path.display())));
    }
    cleanup_socket(path);
    Ok(())
}

fn cleanup_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}
