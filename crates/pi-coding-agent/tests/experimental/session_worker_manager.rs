//! Port of upstream `test/experimental-session-worker-manager.test.ts`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::services::ServiceCall;
use pi_chord::{BACKGROUND_CONTEXT, ChordResult, Json};
use pi_coding_agent::experimental::coordinator::{
    CoordinatorConnectionEvent, CoordinatorEventListener, RemoveListener, WorkerCoordinator,
};
use pi_coding_agent::experimental::session_catalog::SessionCatalogMetadata;
use pi_coding_agent::experimental::session_worker_manager::SessionWorkerManager;
use pi_server::RoutedSessionAttachment;
use serde_json::{Value, json};

fn metadata() -> SessionCatalogMetadata {
    SessionCatalogMetadata {
        id: "session-1".into(),
        created_at: 1.0,
        cwd: "/tmp".into(),
        path: "/tmp/session-1".into(),
    }
}

type OnSend = Arc<dyn Fn(&FakeCoordinator, &str, &Value) + Send + Sync>;

#[derive(Clone, Default)]
struct FakeCoordinator {
    sent: Arc<Mutex<Vec<(String, Value)>>>,
    listeners: Arc<Mutex<Vec<(u64, CoordinatorEventListener)>>>,
    on_send: Arc<Mutex<Option<OnSend>>>,
}

impl FakeCoordinator {
    fn emit(&self, event: CoordinatorConnectionEvent) {
        let listeners: Vec<CoordinatorEventListener> =
            self.listeners.lock().unwrap().iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(&event);
        }
    }

    /// Emit a worker message on a later task, as upstream's `queueMicrotask`.
    fn emit_later(&self, from: &str, payload: Value) {
        let (this, from) = (self.clone(), from.to_string());
        tokio::spawn(async move { this.emit(CoordinatorConnectionEvent::Message { from, payload }) });
    }

    fn set_on_send(&self, on_send: impl Fn(&FakeCoordinator, &str, &Value) + Send + Sync + 'static) {
        *self.on_send.lock().unwrap() = Some(Arc::new(on_send));
    }

    fn sent_types(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        sent.iter().filter_map(|(_, payload)| payload["type"].as_str().map(str::to_string)).collect()
    }
}

#[async_trait]
impl WorkerCoordinator for FakeCoordinator {
    fn control_path(&self) -> PathBuf {
        PathBuf::from("/tmp/control.sock")
    }

    fn server_connection_id(&self) -> String {
        "server-generation-1".into()
    }

    fn was_replaced(&self) -> bool {
        false
    }

    fn on_event(&self, listener: CoordinatorEventListener) -> RemoveListener {
        let mut listeners = self.listeners.lock().unwrap();
        let id = listeners.len() as u64 + 1;
        listeners.push((id, listener));
        let list = self.listeners.clone();
        Box::new(move || list.lock().unwrap().retain(|(existing, _)| *existing != id))
    }

    async fn send(&self, peer_id: &str, payload: Value) -> ChordResult<()> {
        self.sent.lock().unwrap().push((peer_id.to_string(), payload.clone()));
        let on_send = self.on_send.lock().unwrap().clone();
        if let Some(on_send) = on_send {
            on_send(self, peer_id, &payload);
        }
        Ok(())
    }

    async fn broadcast(&self, payload: Value) -> ChordResult<()> {
        if payload["type"] != "discover_workers" {
            return Ok(());
        }
        let metadata = metadata();
        let payload = json!({
            "type": "worker_ready",
            "token": "worker-token",
            "sessionKey": metadata.path,
            "sessionId": metadata.id,
            "pid": 123,
            "metadata": metadata,
            "pluginManifestPaths": [],
        });
        self.emit(CoordinatorConnectionEvent::Message { from: "worker-1".into(), payload });
        Ok(())
    }
}

fn demand_applied(coordinator: &FakeCoordinator, peer_id: &str, payload: &Value) {
    if payload["type"] != "session_demand" {
        return;
    }
    coordinator.emit_later(
        peer_id,
        json!({
            "type": "demand_applied",
            "token": "worker-token",
            "sessionKey": metadata().path,
            "requestId": payload["requestId"],
            "attachmentId": payload["attachmentId"],
            "attached": payload["attached"],
        }),
    );
}

async fn discovered() -> (FakeCoordinator, SessionWorkerManager) {
    let coordinator = FakeCoordinator::default();
    let workers = SessionWorkerManager::new(Arc::new(coordinator.clone()), "/tmp", None, None);
    workers.discover(&HashSet::from(["worker-1".to_string()])).await;
    (coordinator, workers)
}

struct Attached {
    coordinator: FakeCoordinator,
    workers: SessionWorkerManager,
    attachment: Arc<dyn RoutedSessionAttachment>,
}

async fn create_attached_worker() -> Attached {
    let (coordinator, workers) = discovered().await;
    let handle = workers.open_session(&metadata(), &BACKGROUND_CONTEXT, &[]).await.unwrap();
    coordinator.set_on_send(demand_applied);
    let attachment = handle.attach_client(&BACKGROUND_CONTEXT).await.unwrap();
    Attached { coordinator, workers, attachment }
}

fn killer(workers: &SessionWorkerManager) -> Arc<Mutex<Vec<u32>>> {
    let killed = Arc::new(Mutex::new(Vec::new()));
    let record = killed.clone();
    workers.set_process_killer(Arc::new(move |pid| record.lock().unwrap().push(pid)));
    killed
}

#[tokio::test]
async fn adopts_a_discovered_worker_with_its_existing_session_plugin_selection() {
    let (coordinator, workers) = discovered().await;
    assert!(!coordinator.sent_types().contains(&"shutdown".to_string()));
    assert_eq!(workers.worker_pids().len(), 1);
    workers.assert_session_plugin_manifest_paths(&metadata(), &[]).unwrap();
    workers.detach();
}

#[tokio::test]
async fn rejects_a_different_plugin_selection_for_an_active_session_without_stopping_it() {
    let Attached { workers, .. } = create_attached_worker().await;
    let paths = ["/tmp/plugin/chord-facets.json".to_string()];
    let error = workers.assert_session_plugin_manifest_paths(&metadata(), &paths).unwrap_err();
    assert!(error.to_string().contains("active with a different plugin selection"));
    assert_eq!(workers.worker_pids().len(), 1);
    workers.detach();
}

#[tokio::test(start_paused = true)]
async fn compensates_a_timed_out_attachment_before_rejecting_it() {
    let (coordinator, workers) = discovered().await;
    let handle = workers.open_session(&metadata(), &BACKGROUND_CONTEXT, &[]).await.unwrap();
    let demands = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
    let record = demands.clone();
    coordinator.set_on_send(move |coordinator, peer_id, payload| {
        if payload["type"] != "session_demand" {
            return;
        }
        let attached = payload["attached"].as_bool().unwrap();
        record.lock().unwrap().push((payload["attachmentId"].as_str().unwrap().to_string(), attached));
        if !attached {
            demand_applied(coordinator, peer_id, payload);
        }
    });
    let attaching = tokio::spawn(async move { handle.attach_client(&BACKGROUND_CONTEXT).await.map(drop) });
    tokio::time::advance(Duration::from_secs(5)).await;
    let error = attaching.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let demands = demands.lock().unwrap().clone();
    assert_eq!(demands.len(), 2);
    assert!(demands[0].1);
    assert_eq!(demands[1], (demands[0].0.clone(), false));
    workers.detach();
}

#[tokio::test(start_paused = true)]
async fn kills_a_worker_when_timed_out_demand_cannot_be_reconciled() {
    let (coordinator, workers) = discovered().await;
    let killed = killer(&workers);
    let handle = workers.open_session(&metadata(), &BACKGROUND_CONTEXT, &[]).await.unwrap();
    coordinator.set_on_send(|_, _, _| {});
    let attaching = tokio::spawn(async move { handle.attach_client(&BACKGROUND_CONTEXT).await.map(drop) });
    for seconds in [5, 5, 10] {
        tokio::time::advance(Duration::from_secs(seconds)).await;
    }
    let error = attaching.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("worker was terminated"));
    assert_eq!(*killed.lock().unwrap(), [123]);
    assert!(workers.worker_pids().is_empty());
    workers.detach();
}

#[tokio::test(start_paused = true)]
async fn bounds_harness_driven_worker_shutdown() {
    let (coordinator, workers) = discovered().await;
    let killed = killer(&workers);
    let handle = workers.open_session(&metadata(), &BACKGROUND_CONTEXT, &[]).await.unwrap();
    coordinator.set_on_send(demand_applied);
    let attachment = handle.attach_client(&BACKGROUND_CONTEXT).await.unwrap();
    attachment.release(&BACKGROUND_CONTEXT).await.unwrap();
    coordinator.set_on_send(|_, _, _| {});
    let closing = tokio::spawn(async move { handle.close(&BACKGROUND_CONTEXT).await });
    tokio::time::advance(Duration::from_secs(10)).await;
    closing.await.unwrap().unwrap();
    assert_eq!(*killed.lock().unwrap(), [123]);
    assert!(workers.worker_pids().is_empty());
    workers.detach();
}

fn service_call() -> ServiceCall {
    ServiceCall {
        service_id: "test.session".into(),
        instance: None,
        member: "run".into(),
        args: vec![Json::from("Hello")],
    }
}

fn publisher() -> pi_chord::services::ServiceUpdatePublisher {
    Arc::new(|_, _, _| {})
}

fn respond(token: &'static str, scope: Option<Value>) -> impl Fn(&FakeCoordinator, &str, &Value) + Send + Sync {
    move |coordinator, peer_id, payload| {
        if payload["type"] != "operation" {
            return;
        }
        let scope = scope.clone().unwrap_or_else(|| payload["scope"].clone());
        let response = json!({ "type": "operation_result", "requestId": payload["requestId"], "scope": scope, "result": { "accepted": true } });
        let message = json!({ "type": "operation_response", "token": token, "sessionKey": metadata().path, "response": response });
        coordinator.emit_later(peer_id, message);
    }
}

#[tokio::test]
async fn correlates_service_results_to_the_worker_generation_and_attachment() {
    let Attached { coordinator, workers, attachment } = create_attached_worker().await;
    coordinator.set_on_send(respond("worker-token", None));
    let result = attachment.invoke_service(service_call(), publisher(), &BACKGROUND_CONTEXT).await.unwrap();
    assert_eq!(result.map(|value| value.to_serde()), Some(json!({ "accepted": true })));
    let sent = coordinator.sent.lock().unwrap().clone();
    let (_, operation) = sent.iter().find(|(_, payload)| payload["type"] == "operation").unwrap();
    assert_eq!(operation["scope"]["serverConnectionId"], "server-generation-1");
    assert!(operation["scope"]["attachmentId"].is_string());
    assert_eq!(operation["call"], json!({ "serviceId": "test.session", "member": "run", "args": ["Hello"] }));
    workers.detach();
    attachment.release(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn rejects_a_correlated_response_with_mismatched_worker_identity() {
    let Attached { coordinator, workers, attachment } = create_attached_worker().await;
    coordinator.set_on_send(respond("wrong-token", None));
    let error = attachment.invoke_service(service_call(), publisher(), &BACKGROUND_CONTEXT).await.unwrap_err();
    assert!(error.to_string().contains("mismatched operation response"));
    workers.detach();
    attachment.release(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn rejects_a_null_request_scope() {
    let Attached { coordinator, workers, attachment } = create_attached_worker().await;
    coordinator.set_on_send(respond("worker-token", Some(Value::Null)));
    let error = attachment.invoke_service(service_call(), publisher(), &BACKGROUND_CONTEXT).await.unwrap_err();
    assert!(error.to_string().contains("invalid operation response"));
    workers.detach();
    attachment.release(&BACKGROUND_CONTEXT).await.unwrap();
}

#[tokio::test]
async fn rejects_pending_service_calls_on_replacement_without_stopping_the_worker() {
    let Attached { coordinator, workers, attachment } = create_attached_worker().await;
    coordinator.set_on_send(|_, _, _| {});
    let calling =
        tokio::spawn(async move { attachment.invoke_service(service_call(), publisher(), &BACKGROUND_CONTEXT).await });
    while !coordinator.sent_types().contains(&"operation".to_string()) {
        tokio::task::yield_now().await;
    }
    workers.detach();
    let error = calling.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("replaced during a worker operation"));
    assert!(!coordinator.sent_types().contains(&"shutdown".to_string()));
    assert!(workers.worker_pids().is_empty());
}
