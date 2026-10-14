//! Session and process bookkeeping owned by one replaceable server process (upstream
//! `experimental/session-worker-manager.ts`). Worker events are in `session_worker_events`, and
//! attachments and operations in `session_worker_attach`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_chord::services::ServiceProviderUpdate;
use pi_chord::{ChordError, ChordResult, Context, Json};
use serde_json::json;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use super::coordinator::{RemoveListener, WorkerCoordinator};
use super::process::{
    InternalProcess, InternalProcessRole, InternalProcessSpawnOptions, kill_pid, lock, spawn_internal_process,
};
use super::session_catalog::SessionCatalogMetadata;
use super::session_worker::{
    SESSION_WORKER_CONTROL_ADDRESS_ENV, SESSION_WORKER_CONTROL_TOKEN_ENV, SESSION_WORKER_PEER_ID_ENV,
    SESSION_WORKER_SESSION_KEY_ENV, SessionWorkerOptions, WorkerOperationScope,
};

pub(super) const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
pub(super) const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const WORKER_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const WORKER_DEMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The requested plugin selection differs from the one an active or starting worker uses.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{0}")]
pub struct SessionPluginSelectionConflictError(pub String);

/// A model override passed to newly started workers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerModel {
    pub provider: Option<String>,
    pub model: String,
}

pub(super) type SharedResult<T> = Shared<BoxFuture<'static, Result<T, ChordError>>>;

pub(super) struct WorkerRecord {
    pub peer_id: String,
    pub metadata: SessionCatalogMetadata,
    pub pid: u32,
    pub token: String,
    pub plugin_manifest_paths: Vec<String>,
    pub terminated: watch::Sender<Option<Option<ChordError>>>,
    pub attachment_ids: Mutex<HashSet<String>>,
    pub expected_stop: AtomicBool,
    pub stopping: AtomicBool,
    pub stop: Mutex<Option<SharedResult<()>>>,
}

impl WorkerRecord {
    pub async fn wait_terminated(&self) -> Option<ChordError> {
        let mut terminated = self.terminated.subscribe();
        let value = terminated.wait_for(Option::is_some).await;
        value.ok().and_then(|value| value.clone()).flatten()
    }
}

pub(super) struct PendingDemand {
    pub attachment_id: String,
    pub attached: bool,
    pub timer: JoinHandle<()>,
    pub worker: Arc<WorkerRecord>,
    pub sender: oneshot::Sender<ChordResult<()>>,
}

pub(super) struct PendingOperation {
    pub worker: Arc<WorkerRecord>,
    pub scope: WorkerOperationScope,
    pub sender: oneshot::Sender<ChordResult<Option<Json>>>,
}

pub(super) struct ServiceSubscriptionEntry {
    pub worker: Arc<WorkerRecord>,
    pub scope: WorkerOperationScope,
    pub listener: Arc<dyn Fn(ServiceProviderUpdate) + Send + Sync>,
}

pub(super) struct PendingLaunch {
    pub peer_id: String,
    pub token: String,
    pub plugin_manifest_paths: Vec<String>,
    pub child: InternalProcess,
    pub timer: JoinHandle<()>,
    pub promise: SharedResult<Arc<WorkerRecord>>,
    pub sender: Option<oneshot::Sender<ChordResult<Arc<WorkerRecord>>>>,
}

#[derive(Default)]
pub(super) struct State {
    pub workers_by_session: HashMap<String, Arc<WorkerRecord>>,
    pub workers_by_peer: HashMap<String, Arc<WorkerRecord>>,
    pub worker_pids: HashMap<String, u32>,
    pub pending: HashMap<String, PendingLaunch>,
    pub pending_demand: HashMap<String, PendingDemand>,
    pub pending_operations: HashMap<String, PendingOperation>,
    pub service_subscriptions: HashMap<String, ServiceSubscriptionEntry>,
    pub discovery_peers: Option<HashSet<String>>,
    pub resolve_discovery: Option<oneshot::Sender<()>>,
    pub detached: bool,
    pub shutting_down: bool,
}

pub(super) type ProcessKiller = Arc<dyn Fn(u32) + Send + Sync>;

pub(super) struct Inner {
    pub coordinator: Arc<dyn WorkerCoordinator>,
    pub session_dir: String,
    pub model: Option<WorkerModel>,
    pub on_worker_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    pub kill: Mutex<ProcessKiller>,
    pub state: Mutex<State>,
    pub remove_listener: Mutex<Option<RemoveListener>>,
}

/// Session and process bookkeeping owned by one replaceable server process.
#[derive(Clone)]
pub struct SessionWorkerManager(pub(super) Arc<Inner>);

impl SessionWorkerManager {
    pub fn new(
        coordinator: Arc<dyn WorkerCoordinator>,
        session_dir: impl Into<String>,
        model: Option<WorkerModel>,
        on_worker_count_changed: Option<Arc<dyn Fn(usize) + Send + Sync>>,
    ) -> Self {
        let inner = Arc::new(Inner {
            coordinator: coordinator.clone(),
            session_dir: session_dir.into(),
            model,
            on_worker_count_changed,
            kill: Mutex::new(Arc::new(kill_pid)),
            state: Mutex::default(),
            remove_listener: Mutex::default(),
        });
        let weak = Arc::downgrade(&inner);
        let remove = coordinator.on_event(Arc::new(move |event| {
            if let Some(inner) = weak.upgrade() {
                inner.handle_coordinator_event(event);
            }
        }));
        *lock(&inner.remove_listener) = Some(remove);
        Self(inner)
    }

    /// Replace how a timed-out worker is killed (`process.kill(pid, "SIGKILL")` upstream).
    pub fn set_process_killer(&self, kill: Arc<dyn Fn(u32) + Send + Sync>) {
        *lock(&self.0.kill) = kill;
    }

    /// Worker process IDs by Session ID.
    pub fn worker_pids(&self) -> HashMap<String, u32> {
        lock(&self.0.state).worker_pids.clone()
    }

    pub fn tracked_sessions(&self) -> Vec<SessionCatalogMetadata> {
        lock(&self.0.state).workers_by_session.values().map(|worker| worker.metadata.clone()).collect()
    }

    pub fn assert_session_plugin_manifest_paths(
        &self,
        metadata: &SessionCatalogMetadata,
        manifest_paths: &[String],
    ) -> ChordResult<()> {
        self.0.assert_session_plugin_manifest_paths(metadata, manifest_paths)
    }

    pub async fn discover(&self, peer_ids: &HashSet<String>) {
        let wait = {
            let mut state = lock(&self.0.state);
            if state.detached {
                return;
            }
            let undiscovered: HashSet<String> = peer_ids
                .iter()
                .filter(|peer| {
                    !state.workers_by_peer.contains_key(*peer) && !state.pending.values().any(|p| &p.peer_id == *peer)
                })
                .cloned()
                .collect();
            if undiscovered.is_empty() {
                return;
            }
            let (sender, wait) = oneshot::channel();
            state.discovery_peers = Some(undiscovered);
            state.resolve_discovery = Some(sender);
            wait
        };
        let _ = self.0.coordinator.broadcast(json!({ "type": "discover_workers" })).await;
        let _ = tokio::time::timeout(WORKER_DISCOVERY_TIMEOUT, wait).await;
        let mut state = lock(&self.0.state);
        state.discovery_peers = None;
        state.resolve_discovery = None;
    }

    pub async fn close_session(&self, metadata: &SessionCatalogMetadata, context: &Context) -> ChordResult<()> {
        let (worker, pending) = {
            let state = lock(&self.0.state);
            let pending = state.pending.get(&metadata.path).map(|pending| pending.promise.clone());
            (state.workers_by_session.get(&metadata.path).cloned(), pending)
        };
        let worker = match (worker, pending) {
            (Some(worker), _) => Some(worker),
            (None, Some(pending)) => Some(pending.await?),
            (None, None) => None,
        };
        match worker {
            Some(worker) => self.0.stop_worker(&worker, context).await,
            None => Ok(()),
        }
    }

    pub async fn shutdown(&self) {
        let pending: Vec<(String, InternalProcess)> = {
            let mut state = lock(&self.0.state);
            if state.detached || state.shutting_down {
                return;
            }
            state.shutting_down = true;
            state.pending.values().map(|pending| (pending.peer_id.clone(), pending.child.clone())).collect()
        };
        for (peer, _) in &pending {
            let (coordinator, peer) = (self.0.coordinator.clone(), peer.clone());
            tokio::spawn(async move { coordinator.send(&peer, json!({ "type": "shutdown" })).await });
        }
        let children: Vec<InternalProcess> = pending.into_iter().map(|(_, child)| child).collect();
        let stop_pending = async {
            let finished = futures::future::join_all(children.iter().map(InternalProcess::exited));
            if tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, finished).await.is_err() {
                children.iter().for_each(InternalProcess::kill);
                futures::future::join_all(children.iter().map(InternalProcess::exited)).await;
            }
        };
        let workers: Vec<Arc<WorkerRecord>> = lock(&self.0.state).workers_by_session.values().cloned().collect();
        let context = pi_chord::BACKGROUND_CONTEXT.clone();
        let stops = futures::future::join_all(workers.iter().map(|worker| self.0.stop_worker(worker, &context)));
        let _ = futures::join!(stop_pending, stops);
        self.0.detach_state();
    }

    /// Forget workers without stopping them when this server is replaced.
    pub fn detach(&self) {
        let pending: Vec<PendingLaunch> = {
            let mut state = lock(&self.0.state);
            if state.detached {
                return;
            }
            state.detached = true;
            state.pending.drain().map(|(_, pending)| pending).collect()
        };
        for mut pending in pending {
            pending.timer.abort();
            if let Some(sender) = pending.sender.take() {
                let _ = sender.send(Err(ChordError::error("Experimental server was replaced")));
            }
        }
        self.0.reject_operations(|_| true, "Experimental server was replaced during a worker operation");
        self.0.detach_state();
    }
}

impl Inner {
    pub(super) fn assert_session_plugin_manifest_paths(
        &self,
        metadata: &SessionCatalogMetadata,
        manifest_paths: &[String],
    ) -> ChordResult<()> {
        let state = lock(&self.state);
        let conflict = |message: String| Err(ChordError::other(SessionPluginSelectionConflictError(message)));
        if let Some(existing) = state.workers_by_session.get(&metadata.path)
            && existing.plugin_manifest_paths != manifest_paths
        {
            return conflict(format!("Session {} is active with a different plugin selection", metadata.id));
        }
        if let Some(pending) = state.pending.get(&metadata.path)
            && pending.plugin_manifest_paths != manifest_paths
        {
            return conflict(format!("Session {} is starting with a different plugin selection", metadata.id));
        }
        Ok(())
    }

    pub(super) fn launch(
        self: &Arc<Self>,
        metadata: &SessionCatalogMetadata,
        plugin_manifest_paths: &[String],
    ) -> ChordResult<SharedResult<Arc<WorkerRecord>>> {
        let session_key = metadata.path.clone();
        let peer_id = format!("worker-{}", pi_server::random_uuid());
        let token = pi_server::random_uuid();
        let options = SessionWorkerOptions {
            session_dir: self.session_dir.clone(),
            metadata: metadata.clone(),
            provider: self.model.as_ref().and_then(|model| model.provider.clone()),
            model: self.model.as_ref().map(|model| model.model.clone()),
            plugin_manifest_paths: plugin_manifest_paths.to_vec(),
        };
        let argument = serde_json::to_string(&options).map_err(ChordError::other)?;
        let encoded_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(session_key.as_bytes());
        let env = vec![
            (SESSION_WORKER_CONTROL_ADDRESS_ENV.to_string(), self.coordinator.control_path().display().to_string()),
            (SESSION_WORKER_CONTROL_TOKEN_ENV.to_string(), token.clone()),
            (SESSION_WORKER_SESSION_KEY_ENV.to_string(), encoded_key),
            (SESSION_WORKER_PEER_ID_ENV.to_string(), peer_id.clone()),
        ];
        let spawn_options = InternalProcessSpawnOptions { program: None, env };
        let child = spawn_internal_process(InternalProcessRole::SessionWorker, &[argument], spawn_options)?;
        let (sender, receiver) = oneshot::channel();
        let abandoned = || Err(ChordError::error("Session worker startup was abandoned"));
        let promise = async move { receiver.await.unwrap_or_else(|_| abandoned()) }.boxed().shared();
        let weak = Arc::downgrade(self);
        let timer_key = session_key.clone();
        let sleep = tokio::time::sleep(WORKER_STARTUP_TIMEOUT);
        let timer = tokio::spawn(async move {
            sleep.await;
            if let Some(inner) = weak.upgrade() {
                inner.fail_pending(&timer_key, ChordError::error("Session worker startup timed out"));
            }
        });
        let pending = PendingLaunch {
            peer_id: peer_id.clone(),
            token,
            plugin_manifest_paths: plugin_manifest_paths.to_vec(),
            child: child.clone(),
            timer,
            promise: promise.clone(),
            sender: Some(sender),
        };
        lock(&self.state).pending.insert(session_key.clone(), pending);
        self.notify_worker_count_changed();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let description = child.exited().await;
            if let Some(inner) = weak.upgrade() {
                inner.child_exited(&session_key, &peer_id, &description);
            }
        });
        Ok(promise)
    }

    pub(super) fn fail_pending(&self, session_key: &str, error: ChordError) {
        let Some(mut pending) = lock(&self.state).pending.remove(session_key) else { return };
        pending.timer.abort();
        self.notify_worker_count_changed();
        pending.child.kill();
        if let Some(sender) = pending.sender.take() {
            let _ = sender.send(Err(error));
        }
    }

    pub(super) fn notify_worker_count_changed(&self) {
        let count = {
            let state = lock(&self.state);
            state.workers_by_session.len() + state.pending.len()
        };
        if let Some(callback) = &self.on_worker_count_changed {
            callback(count);
        }
    }

    pub(super) fn mark_discovered(&self, peer_id: &str) {
        let mut state = lock(&self.state);
        let Some(peers) = state.discovery_peers.as_mut() else { return };
        if !peers.remove(peer_id) || !peers.is_empty() {
            return;
        }
        if let Some(resolve) = state.resolve_discovery.take() {
            let _ = resolve.send(());
        }
    }

    pub(super) fn detach_state(&self) {
        if let Some(remove) = lock(&self.remove_listener).take() {
            remove();
        }
        self.reject_operations(|_| true, "Experimental server detached during a worker operation");
        let mut state = lock(&self.state);
        for (_, pending) in state.pending_demand.drain() {
            pending.timer.abort();
            let _ = pending.sender.send(Ok(()));
        }
        for (_, pending) in state.pending.drain() {
            pending.timer.abort();
        }
        state.service_subscriptions.clear();
        state.workers_by_peer.clear();
        state.workers_by_session.clear();
        state.worker_pids.clear();
        state.discovery_peers = None;
        if let Some(resolve) = state.resolve_discovery.take() {
            let _ = resolve.send(());
        }
    }
}
