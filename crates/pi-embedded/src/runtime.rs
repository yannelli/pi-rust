//! The runtime: tool registry, session creation and restoration, and lifecycle suspension.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pi_ai::{Message, Model, SystemMessage, UserContent, now_ms};

use crate::host::{EmbeddedHost, EmbeddedTool};
use crate::session::EmbeddedSession;
use crate::types::{LifecycleState, SessionSnapshot, SessionStatus, SessionSummary};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Host and tool registry shared by a runtime's sessions.
pub(crate) struct HostShared {
    pub host: EmbeddedHost,
    pub tools: Mutex<Vec<Arc<dyn EmbeddedTool>>>,
}

impl HostShared {
    /// Registered tools whose required capabilities the host advertises.
    pub fn available_tools(&self) -> Vec<Arc<dyn EmbeddedTool>> {
        let capabilities = &self.host.capabilities;
        lock(&self.tools)
            .iter()
            .filter(|tool| tool.declaration().capabilities.iter().all(|needed| capabilities.contains(needed)))
            .cloned()
            .collect()
    }

    pub fn report(&self, error: String) {
        if let Some(report) = &self.host.listener_error {
            report(error);
        }
    }
}

pub(crate) struct RuntimeShared {
    pub sessions: Mutex<HashMap<String, EmbeddedSession>>,
    unsubscribe: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// Options for [`EmbeddedRuntime::create`].
#[derive(Clone, Debug)]
pub struct CreateSessionOptions {
    pub id: String,
    pub model: Model,
    pub system_prompt: Option<String>,
}

/// Opens in-process sessions over an [`EmbeddedHost`]. Methods that start work spawn tokio tasks,
/// so call them inside a tokio runtime.
pub struct EmbeddedRuntime {
    host: Arc<HostShared>,
    shared: Arc<RuntimeShared>,
}

impl EmbeddedRuntime {
    pub fn new(host: EmbeddedHost) -> anyhow::Result<Self> {
        let tools = host.tools.clone();
        let host = Arc::new(HostShared { host, tools: Mutex::new(Vec::new()) });
        let shared = Arc::new(RuntimeShared { sessions: Mutex::new(HashMap::new()), unsubscribe: Mutex::new(None) });
        let runtime = Self { host: host.clone(), shared: shared.clone() };
        for tool in tools {
            runtime.register_tool(tool)?;
        }
        if let Some(lifecycle) = &host.host.lifecycle {
            let (weak, reporter) = (Arc::downgrade(&shared), Arc::downgrade(&host));
            let handle = tokio::runtime::Handle::try_current().ok();
            let unsubscribe = lifecycle.subscribe(Arc::new(move |state| {
                if state != LifecycleState::Active {
                    suspend_all(&weak, &reporter, handle.as_ref());
                }
            }));
            *lock(&shared.unsubscribe) = Some(unsubscribe);
        }
        Ok(runtime)
    }

    /// Register a tool before sessions open; names must be unique.
    pub fn register_tool(&self, tool: Arc<dyn EmbeddedTool>) -> anyhow::Result<()> {
        if !lock(&self.shared.sessions).is_empty() {
            anyhow::bail!("Register tools before opening sessions");
        }
        let mut tools = lock(&self.host.tools);
        let name = &tool.declaration().name;
        if tools.iter().any(|existing| &existing.declaration().name == name) {
            anyhow::bail!("Duplicate tool: {name}");
        }
        tools.push(tool);
        Ok(())
    }

    /// Tools whose required capabilities the host advertises.
    pub fn available_tools(&self) -> Vec<Arc<dyn EmbeddedTool>> {
        self.host.available_tools()
    }

    pub fn host(&self) -> &EmbeddedHost {
        &self.host.host
    }

    /// Atomically create and open a session; duplicate IDs fail.
    pub async fn create(&self, options: CreateSessionOptions) -> anyhow::Result<EmbeddedSession> {
        if options.id.is_empty() || lock(&self.shared.sessions).contains_key(&options.id) {
            anyhow::bail!("Session id is empty or already open");
        }
        let messages = match options.system_prompt.filter(|prompt| !prompt.is_empty()) {
            Some(prompt) => vec![Message::System(SystemMessage {
                content: UserContent::Text(prompt),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: now_ms(),
            })],
            None => Vec::new(),
        };
        let snapshot = SessionSnapshot {
            version: 1,
            id: options.id.clone(),
            revision: 0,
            sequence: 0,
            status: SessionStatus::Idle,
            model: options.model,
            messages,
            partial: None,
            steering: Vec::new(),
            follow_ups: Vec::new(),
            invocations: Vec::new(),
            usage: Vec::new(),
            context_start: 0,
            context_summary: None,
            error: None,
            needs_response: false,
        };
        self.host.host.store.save(&snapshot, None).await?;
        let session = EmbeddedSession::new(self.host.clone(), self.shared.clone(), snapshot);
        lock(&self.shared.sessions).insert(options.id, session.clone());
        Ok(session)
    }

    /// Open a saved session and checkpoint interrupted work. Starts no requests or tools.
    pub async fn restore(&self, id: &str) -> anyhow::Result<EmbeddedSession> {
        if lock(&self.shared.sessions).contains_key(id) {
            anyhow::bail!("Session is already open");
        }
        let Some(snapshot) = self.host.host.store.load(id).await? else {
            anyhow::bail!("Session not found: {id}");
        };
        if snapshot.version != 1 {
            anyhow::bail!("Unsupported session format");
        }
        if snapshot.status == SessionStatus::Disposed {
            anyhow::bail!("Session is disposed");
        }
        let session = EmbeddedSession::new(self.host.clone(), self.shared.clone(), snapshot);
        session.recover().await?;
        lock(&self.shared.sessions).insert(id.to_string(), session.clone());
        Ok(session)
    }

    /// Persisted summaries, including disposed sessions.
    pub async fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        self.host.host.store.list().await
    }

    /// Dispose open sessions and stop listening to lifecycle events.
    pub async fn dispose(&self) -> anyhow::Result<()> {
        if let Some(unsubscribe) = lock(&self.shared.unsubscribe).take() {
            unsubscribe();
        }
        let sessions: Vec<EmbeddedSession> = lock(&self.shared.sessions).values().cloned().collect();
        let results = futures::future::join_all(sessions.iter().map(EmbeddedSession::dispose)).await;
        results.into_iter().collect()
    }
}

fn suspend_all(shared: &Weak<RuntimeShared>, host: &Weak<HostShared>, handle: Option<&tokio::runtime::Handle>) {
    let Some(shared) = shared.upgrade() else { return };
    let sessions: Vec<EmbeddedSession> = lock(&shared.sessions).values().cloned().collect();
    let handle = handle.cloned().or_else(|| tokio::runtime::Handle::try_current().ok());
    for session in sessions {
        let host_for_error = host.clone();
        let host = host.clone();
        let task = async move {
            if let Err(error) = session.suspend().await
                && let Some(host) = host.upgrade()
            {
                host.report(error.to_string());
            }
        };
        match &handle {
            Some(handle) => drop(handle.spawn(task)),
            None => {
                if let Some(host) = host_for_error.upgrade() {
                    host.report("No tokio runtime is available to suspend sessions".into());
                }
            }
        }
    }
}
