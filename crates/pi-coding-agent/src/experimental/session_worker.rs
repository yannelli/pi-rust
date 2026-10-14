//! Session worker control messages and worker-local lifecycle (upstream `experimental/session-worker.ts`).
//! The process loop lives in `session_worker_run`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::{ChordError, ChordResult, REMOTE_SERVICE_ERROR_CODES};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;

use super::process::lock;
use super::session_catalog::SessionCatalogMetadata;

pub const SESSION_WORKER_CONTROL_ADDRESS_ENV: &str = "PI_SESSION_WORKER_CONTROL_ADDRESS";
pub const SESSION_WORKER_CONTROL_TOKEN_ENV: &str = "PI_SESSION_WORKER_CONTROL_TOKEN";
pub const SESSION_WORKER_SESSION_KEY_ENV: &str = "PI_SESSION_WORKER_SESSION_KEY_BASE64";
pub const SESSION_WORKER_PEER_ID_ENV: &str = "PI_SESSION_WORKER_PEER_ID";
pub const SESSION_WORKER_INITIAL_DEMAND_GRACE_ENV: &str = "__PI_SESSION_WORKER_INITIAL_DEMAND_GRACE_MS";
pub const SESSION_WORKER_ORPHAN_DEMAND_GRACE_ENV: &str = "__PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS";

/// Options passed to a worker as its one JSON argument.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionWorkerOptions {
    pub session_dir: String,
    pub metadata: SessionCatalogMetadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub plugin_manifest_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerOperationScope {
    pub server_connection_id: String,
    pub attachment_id: String,
}

/// A present `null` stays `Some(Null)`; only an absent field is `None`.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkerOperationResponse {
    #[serde(rename_all = "camelCase")]
    OperationResult {
        request_id: String,
        scope: WorkerOperationScope,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    #[serde(rename_all = "camelCase")]
    OperationError {
        request_id: String,
        scope: WorkerOperationScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
        message: String,
    },
}

impl WorkerOperationResponse {
    pub fn request_id(&self) -> &str {
        match self {
            Self::OperationResult { request_id, .. } | Self::OperationError { request_id, .. } => request_id,
        }
    }

    pub fn scope(&self) -> &WorkerOperationScope {
        match self {
            Self::OperationResult { scope, .. } | Self::OperationError { scope, .. } => scope,
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::OperationResult { request_id, .. } => !request_id.is_empty(),
            Self::OperationError { request_id, code, .. } => {
                !request_id.is_empty() && code.as_deref().is_none_or(|code| REMOTE_SERVICE_ERROR_CODES.contains(&code))
            }
        }
    }
}

/// Commands the server sends to a worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionWorkerCommand {
    Shutdown,
    DiscoverWorkers,
    #[serde(rename_all = "camelCase")]
    SessionDemand {
        server_connection_id: String,
        request_id: String,
        attachment_id: String,
        attached: bool,
    },
    #[serde(rename_all = "camelCase")]
    Operation {
        request_id: String,
        scope: WorkerOperationScope,
        call: Value,
    },
    #[serde(rename_all = "camelCase")]
    OperationCancel {
        request_id: String,
        scope: WorkerOperationScope,
    },
}

impl SessionWorkerCommand {
    /// Parse and check a command as upstream's schema does.
    pub fn parse(value: &Value) -> Option<Self> {
        let command: Self = serde_json::from_value(value.clone()).ok()?;
        let object = value.as_object()?;
        let strict = |keys: &[&str]| object.keys().all(|key| keys.contains(&key.as_str()));
        let valid = match &command {
            Self::Operation { request_id, call, .. } => {
                strict(&["type", "requestId", "scope", "call"])
                    && !request_id.is_empty()
                    && pi_chord::services::parse_service_call(&pi_chord::Json::from(call)).is_ok()
            }
            Self::OperationCancel { request_id, .. } => {
                strict(&["type", "requestId", "scope"]) && !request_id.is_empty()
            }
            _ => true,
        };
        valid.then_some(command)
    }
}

/// Events a worker sends to the server.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionWorkerEvent {
    #[serde(rename_all = "camelCase")]
    WorkerReady {
        token: String,
        session_key: String,
        session_id: String,
        pid: u32,
        metadata: SessionCatalogMetadata,
        plugin_manifest_paths: Vec<String>,
    },
    #[serde(rename_all = "camelCase")]
    WorkerFailed { token: String, session_key: String, message: String },
    #[serde(rename_all = "camelCase")]
    DemandApplied { token: String, session_key: String, request_id: String, attachment_id: String, attached: bool },
    #[serde(rename_all = "camelCase")]
    DemandRejected { token: String, session_key: String, request_id: String, message: String },
    #[serde(rename_all = "camelCase")]
    OperationResponse { token: String, session_key: String, response: WorkerOperationResponse },
    #[serde(rename_all = "camelCase")]
    ServiceUpdate {
        token: String,
        session_key: String,
        scope: WorkerOperationScope,
        subscription_id: String,
        update: Value,
    },
}

impl SessionWorkerEvent {
    pub fn parse(value: &Value) -> Option<Self> {
        let event: Self = serde_json::from_value(value.clone()).ok()?;
        let valid = match &event {
            Self::WorkerReady { pid, plugin_manifest_paths, metadata, .. } => {
                *pid >= 1 && !metadata.id.is_empty() && plugin_manifest_paths.iter().all(|path| !path.is_empty())
            }
            Self::OperationResponse { response, .. } => response.valid(),
            Self::ServiceUpdate { subscription_id, .. } => !subscription_id.is_empty(),
            _ => true,
        };
        valid.then_some(event)
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

pub(crate) fn demand_key(server_connection_id: &str, attachment_id: &str) -> String {
    format!("{server_connection_id}\0{attachment_id}")
}

struct Demand {
    server_connection_id: String,
    timer: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct LifecycleState {
    demands: HashMap<String, Demand>,
    harness_active: bool,
    current_server_connection_id: Option<String>,
    initial_timer: Option<JoinHandle<()>>,
    demand_initialized: bool,
    retirement_holds: usize,
    retiring: bool,
}

pub struct WorkerLifecycleOptions {
    pub initial_server_connection_id: Option<String>,
    pub initial_demand_grace: Duration,
    pub orphan_demand_grace: Duration,
    pub on_retire: Box<dyn Fn() + Send + Sync>,
}

/// Worker-local reconciliation of server-generation demand and Harness activity.
#[derive(Clone)]
pub struct WorkerLifecycle(Arc<LifecycleInner>);

struct LifecycleInner {
    state: Mutex<LifecycleState>,
    orphan_demand_grace: Duration,
    on_retire: Box<dyn Fn() + Send + Sync>,
}

/// Releases one retirement hold. Idempotent.
pub struct RetirementHold {
    lifecycle: Option<WorkerLifecycle>,
}

impl RetirementHold {
    pub fn release(&mut self) {
        if let Some(lifecycle) = self.lifecycle.take() {
            lock(&lifecycle.0.state).retirement_holds -= 1;
            lifecycle.reconcile();
        }
    }
}

impl Drop for RetirementHold {
    fn drop(&mut self) {
        self.release();
    }
}

impl WorkerLifecycle {
    pub fn new(options: WorkerLifecycleOptions) -> Self {
        let state =
            LifecycleState { current_server_connection_id: options.initial_server_connection_id, ..Default::default() };
        let inner = Arc::new(LifecycleInner {
            state: Mutex::new(state),
            orphan_demand_grace: options.orphan_demand_grace,
            on_retire: options.on_retire,
        });
        let weak = Arc::downgrade(&inner);
        let grace = options.initial_demand_grace;
        let timer = tokio::spawn(async move {
            tokio::time::sleep(grace).await;
            let Some(inner) = weak.upgrade() else { return };
            {
                let mut state = lock(&inner.state);
                state.initial_timer = None;
                state.demand_initialized = true;
            }
            WorkerLifecycle(inner).reconcile();
        });
        lock(&inner.state).initial_timer = Some(timer);
        Self(inner)
    }

    pub fn server_connected(&self, server_connection_id: &str) {
        let mut state = lock(&self.0.state);
        state.current_server_connection_id = Some(server_connection_id.to_string());
        for demand in state.demands.values_mut() {
            if demand.server_connection_id == server_connection_id
                && let Some(timer) = demand.timer.take()
            {
                timer.abort();
            }
        }
    }

    pub fn server_disconnected(&self, server_connection_id: &str) {
        let mut state = lock(&self.0.state);
        if state.current_server_connection_id.as_deref() == Some(server_connection_id) {
            state.current_server_connection_id = None;
        }
        let keys: Vec<String> = state
            .demands
            .iter()
            .filter(|(_, demand)| demand.server_connection_id == server_connection_id && demand.timer.is_none())
            .map(|(key, _)| key.clone())
            .collect();
        for key in keys {
            let weak = Arc::downgrade(&self.0);
            let grace = self.0.orphan_demand_grace;
            let timer_key = key.clone();
            let timer = tokio::spawn(async move {
                tokio::time::sleep(grace).await;
                let Some(inner) = weak.upgrade() else { return };
                let removed = {
                    let mut state = lock(&inner.state);
                    let orphaned = state.demands.get(&timer_key).is_some_and(|demand| demand.timer.is_some());
                    orphaned && state.demands.remove(&timer_key).is_some()
                };
                if removed {
                    WorkerLifecycle(inner).reconcile();
                }
            });
            if let Some(demand) = state.demands.get_mut(&key) {
                demand.timer = Some(timer);
            }
        }
    }

    pub fn begin_request(&self, server_connection_id: &str, attachment_id: &str) -> ChordResult<RetirementHold> {
        {
            let state = lock(&self.0.state);
            if state.retiring {
                return Err(ChordError::error("Session worker is retiring"));
            }
            if state.current_server_connection_id.as_deref() != Some(server_connection_id) {
                return Err(ChordError::error("Session worker received a request from a stale server generation"));
            }
            let demand = state.demands.get(&demand_key(server_connection_id, attachment_id));
            if demand.is_none_or(|demand| demand.timer.is_some()) {
                return Err(ChordError::error("Session worker request does not match the active attachment"));
            }
        }
        Ok(self.hold_retirement())
    }

    pub fn hold_retirement(&self) -> RetirementHold {
        lock(&self.0.state).retirement_holds += 1;
        RetirementHold { lifecycle: Some(self.clone()) }
    }

    pub fn set_demand(&self, server_connection_id: &str, attachment_id: &str, attached: bool) -> ChordResult<()> {
        {
            let mut state = lock(&self.0.state);
            if state.retiring {
                return Err(ChordError::error("Session worker is retiring"));
            }
            if state.current_server_connection_id.as_deref() != Some(server_connection_id) {
                return Err(ChordError::error("Session worker received demand from a stale server generation"));
            }
            state.demand_initialized = true;
            if let Some(timer) = state.initial_timer.take() {
                timer.abort();
            }
            let key = demand_key(server_connection_id, attachment_id);
            if let Some(timer) = state.demands.get_mut(&key).and_then(|previous| previous.timer.take()) {
                timer.abort();
            }
            if attached {
                let demand = Demand { server_connection_id: server_connection_id.to_string(), timer: None };
                state.demands.insert(key, demand);
            } else {
                state.demands.remove(&key);
            }
        }
        self.reconcile();
        Ok(())
    }

    /// Live Harness tasks, such as runs and compactions, hold the worker until they end.
    pub fn set_harness_active(&self, active: bool) {
        lock(&self.0.state).harness_active = active;
        if !active {
            self.reconcile();
        }
    }

    pub fn close(&self) {
        let mut state = lock(&self.0.state);
        if let Some(timer) = state.initial_timer.take() {
            timer.abort();
        }
        for demand in state.demands.values_mut() {
            if let Some(timer) = demand.timer.take() {
                timer.abort();
            }
        }
        state.demands.clear();
    }

    fn reconcile(&self) {
        {
            let mut state = lock(&self.0.state);
            if state.retiring
                || !state.demand_initialized
                || state.retirement_holds != 0
                || state.harness_active
                || !state.demands.is_empty()
            {
                return;
            }
            state.retiring = true;
        }
        (self.0.on_retire)();
    }
}

/// Read a non-negative lifecycle delay from the environment.
pub fn lifecycle_delay(name: &str, fallback: Duration) -> ChordResult<Duration> {
    let Ok(value) = std::env::var(name) else { return Ok(fallback) };
    match value.trim().parse::<u64>() {
        Ok(ms) if ms <= 9_007_199_254_740_991 => Ok(Duration::from_millis(ms)),
        _ => Err(ChordError::error(format!("{name} must be a non-negative safe integer"))),
    }
}
