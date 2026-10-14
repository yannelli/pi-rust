//! Routed Session handles, client attachments, demand updates, worker operations, and worker stops for
//! the Session worker manager.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::FutureExt;
use pi_chord::services::{
    ServiceCall, ServiceControlCall, ServiceUpdatePublisher, create_service_unsubscribe_call,
    decode_service_control_call,
};
use pi_chord::{BACKGROUND_CONTEXT, BoxFuture, ChordError, ChordResult, Context, Json};
use pi_server::{RoutedSessionAttachment, RoutedSessionHandle};
use serde_json::json;
use tokio::sync::oneshot;

use super::process::lock;
use super::session_catalog::SessionCatalogMetadata;
use super::session_worker::WorkerOperationScope;
use super::session_worker_manager::{
    Inner, PendingDemand, PendingOperation, ServiceSubscriptionEntry, SessionWorkerManager, SharedResult,
    WORKER_DEMAND_TIMEOUT, WORKER_SHUTDOWN_TIMEOUT, WorkerRecord,
};

pub(super) fn scoped_key(scope: &WorkerOperationScope, subscription_id: &str) -> String {
    format!("{}\0{}\0{subscription_id}", scope.server_connection_id, scope.attachment_id)
}

fn stopping() -> ChordError {
    ChordError::error("Experimental Session worker is stopping")
}

impl SessionWorkerManager {
    pub async fn open_session(
        &self,
        metadata: &SessionCatalogMetadata,
        _context: &Context,
        plugin_manifest_paths: &[String],
    ) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        let inner = &self.0;
        let (existing, pending) = {
            let state = lock(&inner.state);
            if state.detached || state.shutting_down {
                return Err(ChordError::error("Experimental server is shutting down"));
            }
            let pending = state.pending.get(&metadata.path).map(|pending| pending.promise.clone());
            (state.workers_by_session.get(&metadata.path).cloned(), pending)
        };
        inner.assert_session_plugin_manifest_paths(metadata, plugin_manifest_paths)?;
        let worker = match (existing, pending) {
            (Some(worker), _) => worker,
            (None, Some(pending)) => pending.await?,
            (None, None) => inner.launch(metadata, plugin_manifest_paths)?.await?,
        };
        Ok(Arc::new(WorkerHandle { inner: inner.clone(), worker }))
    }
}

struct WorkerHandle {
    inner: Arc<Inner>,
    worker: Arc<WorkerRecord>,
}

#[async_trait]
impl RoutedSessionHandle for WorkerHandle {
    async fn attach_client(&self, context: &Context) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        self.inner.attach_client(&self.worker, context).await
    }

    fn terminated(&self) -> Option<BoxFuture<Option<ChordError>>> {
        let worker = self.worker.clone();
        Some(Box::pin(async move { worker.wait_terminated().await }))
    }

    async fn close(&self, context: &Context) -> ChordResult<()> {
        self.inner.stop_worker(&self.worker, context).await
    }
}

struct WorkerAttachment {
    inner: Arc<Inner>,
    worker: Arc<WorkerRecord>,
    scope: WorkerOperationScope,
    released: AtomicBool,
}

#[async_trait]
impl RoutedSessionAttachment for WorkerAttachment {
    async fn invoke_service(
        &self,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        self.inner.invoke_service(&self.worker, &self.scope, call, publish, context).await
    }

    async fn release(&self, context: &Context) -> ChordResult<()> {
        if self.released.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let attachment_id = &self.scope.attachment_id;
        if lock(&self.inner.state).detached || !lock(&self.worker.attachment_ids).contains(attachment_id) {
            return Ok(());
        }
        let result = self.inner.apply_demand(&self.worker, attachment_id, false, true, context).await;
        lock(&self.worker.attachment_ids).remove(attachment_id);
        let (worker, scope) = (&self.worker, &self.scope);
        self.inner.remove_service_subscriptions(|entry| Arc::ptr_eq(&entry.worker, worker) && &entry.scope == scope);
        match result {
            Err(_) if lock(&self.inner.state).detached || self.inner.coordinator.was_replaced() => Ok(()),
            other => other,
        }
    }
}

impl Inner {
    fn is_current(&self, worker: &Arc<WorkerRecord>) -> bool {
        lock(&self.state).workers_by_peer.get(&worker.peer_id).is_some_and(|current| Arc::ptr_eq(current, worker))
    }

    async fn attach_client(
        self: &Arc<Self>,
        worker: &Arc<WorkerRecord>,
        context: &Context,
    ) -> ChordResult<Arc<dyn RoutedSessionAttachment>> {
        {
            let state = lock(&self.state);
            if state.detached || state.shutting_down || worker.stopping.load(Ordering::SeqCst) {
                return Err(stopping());
            }
        }
        if !self.is_current(worker) {
            return Err(ChordError::error("Experimental Session worker is no longer available"));
        }
        let attachment_id = pi_server::random_uuid();
        lock(&worker.attachment_ids).insert(attachment_id.clone());
        if let Err(error) = self.apply_demand(worker, &attachment_id, true, true, context).await {
            lock(&worker.attachment_ids).remove(&attachment_id);
            return Err(error);
        }
        let scope = self.operation_scope(worker, &attachment_id)?;
        Ok(Arc::new(WorkerAttachment {
            inner: self.clone(),
            worker: worker.clone(),
            scope,
            released: AtomicBool::new(false),
        }))
    }

    async fn invoke_service(
        self: &Arc<Self>,
        worker: &Arc<WorkerRecord>,
        scope: &WorkerOperationScope,
        call: ServiceCall,
        publish: ServiceUpdatePublisher,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        let control = decode_service_control_call(&call);
        let mut added = None;
        if let Some(ServiceControlCall::Subscribe { subscription_id, .. }) = &control {
            let key = scoped_key(scope, subscription_id);
            let mut state = lock(&self.state);
            if state.service_subscriptions.contains_key(&key) {
                return Err(ChordError::error("Service subscription ID is already active"));
            }
            let id = subscription_id.clone();
            let listener = Arc::new(move |update| publish(&id, update, &pi_chord::TODO_CONTEXT));
            let entry = ServiceSubscriptionEntry { worker: worker.clone(), scope: scope.clone(), listener };
            state.service_subscriptions.insert(key.clone(), entry);
            added = Some((key, subscription_id.clone()));
        }
        let response = match self.invoke(worker, scope, &call, context).await {
            Ok(response) => response,
            Err(error) => {
                if let Some((key, subscription_id)) = added {
                    lock(&self.state).service_subscriptions.remove(&key);
                    let (this, worker, scope) = (self.clone(), worker.clone(), scope.clone());
                    tokio::spawn(async move {
                        let call = create_service_unsubscribe_call(&subscription_id);
                        let _ = this.invoke(&worker, &scope, &call, &BACKGROUND_CONTEXT).await;
                    });
                }
                return Err(error);
            }
        };
        if let Some(ServiceControlCall::Unsubscribe { subscription_id }) = control {
            lock(&self.state).service_subscriptions.remove(&scoped_key(scope, &subscription_id));
        }
        Ok(response)
    }

    pub(super) async fn apply_demand(
        self: &Arc<Self>,
        worker: &Arc<WorkerRecord>,
        attachment_id: &str,
        attached: bool,
        compensate_on_timeout: bool,
        _context: &Context,
    ) -> ChordResult<()> {
        if worker.stopping.load(Ordering::SeqCst) || !self.is_current(worker) {
            return Err(stopping());
        }
        let request_id = pi_server::random_uuid();
        let (sender, applied) = oneshot::channel();
        let weak = Arc::downgrade(self);
        let timer_request = request_id.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(WORKER_DEMAND_TIMEOUT).await;
            let Some(inner) = weak.upgrade() else { return };
            if compensate_on_timeout {
                tokio::spawn(inner.reconcile_demand_timeout(timer_request));
            } else {
                inner.reject_demand(&timer_request, ChordError::error("Session worker demand update timed out"));
            }
        });
        let pending =
            PendingDemand { attachment_id: attachment_id.to_string(), attached, timer, worker: worker.clone(), sender };
        lock(&self.state).pending_demand.insert(request_id.clone(), pending);
        let message = json!({
            "type": "session_demand",
            "serverConnectionId": self.coordinator.server_connection_id(),
            "requestId": request_id,
            "attachmentId": attachment_id,
            "attached": attached,
        });
        if let Err(error) = self.coordinator.send(&worker.peer_id, message).await {
            self.reject_demand(&request_id, error);
        }
        applied.await.unwrap_or_else(|_| Err(ChordError::error("Session worker demand update was abandoned")))
    }

    /// Worker operations have no wall-clock timeout: completion, disconnect, replacement, or shutdown settles them.
    async fn invoke(
        &self,
        worker: &Arc<WorkerRecord>,
        scope: &WorkerOperationScope,
        call: &ServiceCall,
        context: &Context,
    ) -> ChordResult<Option<Json>> {
        self.operation_scope(worker, &scope.attachment_id)?;
        let signal = context.abort_signal();
        if let Some(signal) = signal.as_ref().filter(|signal| signal.aborted()) {
            return Err(signal.reason().unwrap_or(ChordError::Aborted));
        }
        let request_id = pi_server::random_uuid();
        let (sender, result) = oneshot::channel();
        let pending = PendingOperation { worker: worker.clone(), scope: scope.clone(), sender };
        lock(&self.state).pending_operations.insert(request_id.clone(), pending);
        let message =
            json!({ "type": "operation", "requestId": request_id, "scope": scope, "call": call.to_json().to_serde() });
        let send = self.coordinator.send(&worker.peer_id, message);
        if let Err(error) = send.await {
            self.reject_operation(&request_id, error);
        }
        let abandoned = || Err(ChordError::error("Session worker operation was abandoned"));
        let Some(signal) = signal else { return result.await.unwrap_or_else(|_| abandoned()) };
        tokio::select! {
            settled = result => settled.unwrap_or_else(|_| abandoned()),
            reason = signal.cancelled() => {
                if lock(&self.state).pending_operations.contains_key(&request_id) {
                    let cancel = json!({ "type": "operation_cancel", "requestId": request_id, "scope": scope });
                    let (coordinator, peer) = (self.coordinator.clone(), worker.peer_id.clone());
                    tokio::spawn(async move { coordinator.send(&peer, cancel).await });
                    lock(&self.state).pending_operations.remove(&request_id);
                }
                Err(reason)
            }
        }
    }

    fn operation_scope(&self, worker: &Arc<WorkerRecord>, attachment_id: &str) -> ChordResult<WorkerOperationScope> {
        {
            let state = lock(&self.state);
            if state.detached || state.shutting_down || worker.stopping.load(Ordering::SeqCst) {
                return Err(stopping());
            }
        }
        if !self.is_current(worker) || !lock(&worker.attachment_ids).contains(attachment_id) {
            return Err(ChordError::error("Experimental Session worker has no active attachment"));
        }
        Ok(WorkerOperationScope {
            server_connection_id: self.coordinator.server_connection_id(),
            attachment_id: attachment_id.to_string(),
        })
    }

    pub(super) fn stop_worker(self: &Arc<Self>, worker: &Arc<WorkerRecord>, _context: &Context) -> SharedResult<()> {
        if lock(&self.state).detached || !self.is_current(worker) {
            return futures::future::ready(Ok(())).boxed().shared();
        }
        let mut stop = lock(&worker.stop);
        if let Some(stop) = stop.as_ref() {
            return stop.clone();
        }
        let future = self.clone().stop_worker_internal(worker.clone()).boxed().shared();
        *stop = Some(future.clone());
        tokio::spawn(future.clone());
        future
    }

    async fn stop_worker_internal(self: Arc<Self>, worker: Arc<WorkerRecord>) -> ChordResult<()> {
        self.reject_operations(|pending| Arc::ptr_eq(&pending.worker, &worker), "Session worker is stopping");
        worker.stopping.store(true, Ordering::SeqCst);
        worker.expected_stop.store(true, Ordering::SeqCst);
        let (coordinator, peer) = (self.coordinator.clone(), worker.peer_id.clone());
        tokio::spawn(async move { coordinator.send(&peer, json!({ "type": "shutdown" })).await });
        if tokio::time::timeout(WORKER_SHUTDOWN_TIMEOUT, worker.wait_terminated()).await.is_err() {
            let kill = lock(&self.kill).clone();
            kill(worker.pid);
            self.remove_worker(&worker, None);
            worker.wait_terminated().await;
        }
        Ok(())
    }

    fn reconcile_demand_timeout(self: Arc<Self>, request_id: String) -> BoxFuture<()> {
        Box::pin(async move { self.reconcile_demand_timeout_now(&request_id).await })
    }

    async fn reconcile_demand_timeout_now(self: &Arc<Self>, request_id: &str) {
        let Some(pending) = lock(&self.state).pending_demand.remove(request_id) else { return };
        pending.timer.abort();
        let timeout = || ChordError::error("Session worker demand update timed out");
        let cleanup =
            self.apply_demand(&pending.worker, &pending.attachment_id, false, false, &BACKGROUND_CONTEXT).await;
        let result = match cleanup {
            Ok(()) if pending.attached => Err(timeout()),
            Ok(()) => Ok(()),
            Err(cleanup_error) => match self.stop_worker(&pending.worker, &BACKGROUND_CONTEXT).await {
                Err(stop_error) => Err(ChordError::Aggregate {
                    message: "Session worker demand reconciliation and termination failed".into(),
                    errors: vec![timeout(), cleanup_error, stop_error],
                }),
                Ok(()) => Err(ChordError::Aggregate {
                    message: "Session worker demand reconciliation failed; worker was terminated".into(),
                    errors: vec![timeout(), cleanup_error],
                }),
            },
        };
        let _ = pending.sender.send(result);
    }

    fn reject_demand(&self, request_id: &str, error: ChordError) {
        let Some(pending) = lock(&self.state).pending_demand.remove(request_id) else { return };
        pending.timer.abort();
        let _ = pending.sender.send(Err(error));
    }

    pub(super) fn reject_operation(&self, request_id: &str, error: ChordError) {
        let pending = lock(&self.state).pending_operations.remove(request_id);
        if let Some(pending) = pending {
            let _ = pending.sender.send(Err(error));
        }
    }

    pub(super) fn reject_operations(&self, matches: impl Fn(&PendingOperation) -> bool, message: &str) {
        let rejected: Vec<PendingOperation> = {
            let mut state = lock(&self.state);
            let ids: Vec<String> = state
                .pending_operations
                .iter()
                .filter(|(_, pending)| matches(pending))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter().filter_map(|id| state.pending_operations.remove(id)).collect()
        };
        for pending in rejected {
            let _ = pending.sender.send(Err(ChordError::error(message)));
        }
    }

    pub(super) fn remove_worker(&self, worker: &Arc<WorkerRecord>, error: Option<ChordError>) {
        if !self.is_current(worker) {
            return;
        }
        let message = "Session worker disconnected during an operation";
        self.reject_operations(|pending| Arc::ptr_eq(&pending.worker, worker), message);
        self.remove_service_subscriptions(|entry| Arc::ptr_eq(&entry.worker, worker));
        let demands: Vec<String> = lock(&self.state)
            .pending_demand
            .iter()
            .filter(|(_, pending)| Arc::ptr_eq(&pending.worker, worker))
            .map(|(id, _)| id.clone())
            .collect();
        for id in demands {
            self.reject_demand(&id, ChordError::error("Session worker disconnected during demand update"));
        }
        {
            let mut state = lock(&self.state);
            state.workers_by_peer.remove(&worker.peer_id);
            state.workers_by_session.remove(&worker.metadata.path);
            if state.worker_pids.get(&worker.metadata.id) == Some(&worker.pid) {
                state.worker_pids.remove(&worker.metadata.id);
            }
        }
        worker.terminated.send_replace(Some(error));
        self.notify_worker_count_changed();
    }

    pub(super) fn remove_service_subscriptions(&self, matches: impl Fn(&ServiceSubscriptionEntry) -> bool) {
        lock(&self.state).service_subscriptions.retain(|_, entry| !matches(entry));
    }
}
