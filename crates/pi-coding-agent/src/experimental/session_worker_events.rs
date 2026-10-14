//! Coordinator events for the Session worker manager: readiness, failures, demand acknowledgements,
//! operation responses, and service updates.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::services::parse_service_provider_update;
use pi_chord::{ChordError, ChordResult, Json, RemoteServiceErrorCode};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::coordinator::CoordinatorConnectionEvent;
use super::process::lock;
use super::session_worker::{SessionWorkerEvent, WorkerOperationResponse, WorkerOperationScope};
use super::session_worker_attach::scoped_key;
use super::session_worker_manager::{Inner, WorkerRecord};

impl Inner {
    pub(super) fn handle_coordinator_event(self: &Arc<Self>, event: &CoordinatorConnectionEvent) {
        if lock(&self.state).detached {
            return;
        }
        let (from, payload) = match event {
            CoordinatorConnectionEvent::PeerDisconnected { peer_id } => {
                self.mark_discovered(peer_id);
                let worker = lock(&self.state).workers_by_peer.get(peer_id).cloned();
                if let Some(worker) = worker {
                    let message = format!("Session worker {} disconnected unexpectedly", worker.metadata.id);
                    let error = (!worker.expected_stop.load(Ordering::SeqCst)).then(|| ChordError::error(message));
                    self.remove_worker(&worker, error);
                }
                let pending = lock(&self.state)
                    .pending
                    .iter()
                    .find(|(_, pending)| &pending.peer_id == peer_id)
                    .map(|(key, _)| key.clone());
                if let Some(key) = pending {
                    self.fail_pending(&key, ChordError::error("Session worker disconnected during startup"));
                }
                return;
            }
            CoordinatorConnectionEvent::PeerConnected { .. } => return,
            CoordinatorConnectionEvent::Message { from, payload } => (from, payload),
        };
        let Some(message) = SessionWorkerEvent::parse(payload) else {
            if payload.get("type").and_then(Value::as_str) == Some("operation_response") {
                let worker = lock(&self.state).workers_by_peer.get(from).cloned();
                if let Some(worker) = worker {
                    let message = "Session worker returned an invalid operation response";
                    self.reject_operations(|pending| Arc::ptr_eq(&pending.worker, &worker), message);
                }
            }
            return;
        };
        match message {
            SessionWorkerEvent::WorkerFailed { token, session_key, message } => {
                let matches = lock(&self.state)
                    .pending
                    .get(&session_key)
                    .is_some_and(|pending| &pending.peer_id == from && pending.token == token);
                if matches {
                    self.fail_pending(&session_key, ChordError::error(format!("Session worker failed: {message}")));
                }
            }
            SessionWorkerEvent::DemandApplied { token, session_key, request_id, attachment_id, attached } => {
                let applied = Some((attachment_id.as_str(), attached));
                self.settle_demand(from, &token, &session_key, &request_id, applied, Ok(()));
            }
            SessionWorkerEvent::DemandRejected { token, session_key, request_id, message } => {
                let error = ChordError::error(format!("Session worker rejected demand: {message}"));
                self.settle_demand(from, &token, &session_key, &request_id, None, Err(error));
            }
            SessionWorkerEvent::OperationResponse { token, session_key, response } => {
                self.handle_operation_response(from, &token, &session_key, response);
            }
            SessionWorkerEvent::ServiceUpdate { token, session_key, scope, subscription_id, update } => {
                self.handle_service_event(from, &token, &session_key, &scope, &subscription_id, &update);
            }
            ready @ SessionWorkerEvent::WorkerReady { .. } => self.record_ready_worker(from, ready),
        }
    }

    fn settle_demand(
        &self,
        from: &str,
        token: &str,
        session_key: &str,
        request_id: &str,
        applied: Option<(&str, bool)>,
        result: ChordResult<()>,
    ) {
        let mut state = lock(&self.state);
        let Some(pending) = state.pending_demand.get(request_id) else { return };
        let mismatched = pending.worker.peer_id != from
            || pending.worker.token != token
            || pending.worker.metadata.path != session_key
            || applied.is_some_and(|(attachment_id, attached)| {
                pending.attachment_id != attachment_id || pending.attached != attached
            });
        if mismatched {
            return;
        }
        let Some(pending) = state.pending_demand.remove(request_id) else { return };
        drop(state);
        pending.timer.abort();
        let _ = pending.sender.send(result);
    }

    fn handle_operation_response(
        &self,
        peer_id: &str,
        token: &str,
        session_key: &str,
        response: WorkerOperationResponse,
    ) {
        let request_id = response.request_id().to_string();
        let mut state = lock(&self.state);
        let Some(pending) = state.pending_operations.get(&request_id) else { return };
        let scope = response.scope();
        if pending.worker.peer_id != peer_id
            || pending.worker.token != token
            || pending.worker.metadata.path != session_key
            || scope != &pending.scope
        {
            drop(state);
            self.reject_operation(
                &request_id,
                ChordError::error("Session worker returned a mismatched operation response"),
            );
            return;
        }
        let Some(pending) = state.pending_operations.remove(&request_id) else { return };
        drop(state);
        let result = match response {
            WorkerOperationResponse::OperationError { code, message, .. } => {
                Err(match code.as_deref().and_then(RemoteServiceErrorCode::parse) {
                    Some(code) => pi_server::ServerError::new(pi_server::ServerErrorCode::Remote(code), message).into(),
                    None => ChordError::error(format!("Session worker operation failed: {message}")),
                })
            }
            WorkerOperationResponse::OperationResult { result, .. } => Ok(result.as_ref().map(Json::from)),
        };
        let _ = pending.sender.send(result);
    }

    fn handle_service_event(
        &self,
        peer_id: &str,
        token: &str,
        session_key: &str,
        scope: &WorkerOperationScope,
        subscription_id: &str,
        update: &Value,
    ) {
        let listener = {
            let state = lock(&self.state);
            let Some(entry) = state.service_subscriptions.get(&scoped_key(scope, subscription_id)) else { return };
            if entry.worker.peer_id != peer_id
                || entry.worker.token != token
                || entry.worker.metadata.path != session_key
                || &entry.scope != scope
            {
                return;
            }
            entry.listener.clone()
        };
        if let Ok(parsed) = parse_service_provider_update(&Json::from(update)) {
            listener(parsed);
        }
    }

    fn record_ready_worker(self: &Arc<Self>, peer_id: &str, message: SessionWorkerEvent) {
        let SessionWorkerEvent::WorkerReady { token, session_key, session_id, pid, metadata, plugin_manifest_paths } =
            message
        else {
            return;
        };
        if session_key != metadata.path
            || session_id != metadata.id
            || !Path::new(&metadata.cwd).is_absolute()
            || !Path::new(&metadata.path).is_absolute()
        {
            return;
        }
        self.mark_discovered(peer_id);
        let mut state = lock(&self.state);
        let pending = state.pending.get(&session_key);
        if pending.is_some_and(|p| p.peer_id == peer_id && plugin_manifest_paths != p.plugin_manifest_paths) {
            drop(state);
            self.fail_pending(&session_key, ChordError::error("Session worker started with stale plugin packages"));
            return;
        }
        let existing = state.workers_by_session.get(&session_key).cloned();
        if existing.as_ref().is_some_and(|existing| existing.peer_id != peer_id) {
            drop(state);
            let (coordinator, peer) = (self.coordinator.clone(), peer_id.to_string());
            tokio::spawn(async move { coordinator.send(&peer, json!({ "type": "shutdown" })).await });
            return;
        }
        let stale = pending.is_some_and(|p| p.peer_id != peer_id || p.token != token || p.child.pid() != pid);
        if stale || existing.is_some() {
            return;
        }
        let worker = Arc::new(WorkerRecord {
            peer_id: peer_id.to_string(),
            metadata,
            pid,
            token,
            plugin_manifest_paths,
            terminated: watch::channel(None).0,
            attachment_ids: Mutex::default(),
            expected_stop: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            stop: Mutex::new(None),
        });
        state.workers_by_session.insert(session_key.clone(), worker.clone());
        state.workers_by_peer.insert(peer_id.to_string(), worker.clone());
        state.worker_pids.insert(session_id, pid);
        let pending = state.pending.remove(&session_key);
        drop(state);
        if let Some(mut pending) = pending {
            pending.timer.abort();
            if let Some(sender) = pending.sender.take() {
                let _ = sender.send(Ok(worker));
            }
        }
        self.notify_worker_count_changed();
    }

    pub(super) fn child_exited(self: &Arc<Self>, session_key: &str, peer_id: &str, description: &str) {
        let pending = lock(&self.state).pending.get(session_key).is_some_and(|pending| pending.peer_id == peer_id);
        if pending {
            let error = ChordError::error(format!("Session worker exited before readiness ({description})"));
            self.fail_pending(session_key, error);
            return;
        }
        let worker = lock(&self.state).workers_by_peer.get(peer_id).cloned();
        if let Some(worker) = worker {
            let message = format!("Session worker {} exited unexpectedly ({description})", worker.metadata.id);
            let error = (!worker.expected_stop.load(Ordering::SeqCst)).then(|| ChordError::error(message));
            self.remove_worker(&worker, error);
        }
    }
}
