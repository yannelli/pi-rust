//! One open session: checkpointed state changes, ordered events, and host commands.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_ai::transcript::current_system_prompt;
use pi_ai::{Message, Model, StopReason, SystemMessage, UserContent, now_ms};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::runtime::{HostShared, RuntimeShared, lock};
use crate::types::{EventPayload, InvocationPhase, Reconciliation, SessionEvent, SessionSnapshot, SessionStatus};

pub type SessionListener = Arc<dyn Fn(&SessionEvent) + Send + Sync>;
pub(crate) type Execution = Shared<BoxFuture<'static, Result<(), String>>>;

#[derive(Default)]
pub(crate) struct ExecState {
    pub execution: Option<Execution>,
    pub controller: Option<CancellationToken>,
    pub generation: u64,
}

pub(crate) struct SessionInner {
    pub host: Arc<HostShared>,
    /// Strong, so an open session keeps lifecycle handling alive; dispose releases it.
    runtime: Arc<RuntimeShared>,
    pub state: Mutex<SessionSnapshot>,
    writes: tokio::sync::Mutex<()>,
    pub storage_error: Mutex<Option<String>>,
    pub exec: Mutex<ExecState>,
    pub stopping: Mutex<SessionStatus>,
    pub approvals: Mutex<HashMap<String, oneshot::Sender<bool>>>,
    listeners: Mutex<Vec<(u64, SessionListener)>>,
    next_listener: AtomicU64,
}

/// A handle to an open session. Clones share the session.
#[derive(Clone)]
pub struct EmbeddedSession {
    pub(crate) inner: Arc<SessionInner>,
}

/// Removes a listener added by [`EmbeddedSession::subscribe`].
pub struct Subscription {
    session: Weak<SessionInner>,
    id: u64,
}

impl Subscription {
    pub fn unsubscribe(self) {
        if let Some(session) = self.session.upgrade() {
            lock(&session.listeners).retain(|(id, _)| *id != self.id);
        }
    }
}

pub(crate) fn is_failed(message: &Message) -> bool {
    matches!(message, Message::Assistant(m) if matches!(m.stop_reason, StopReason::Error | StopReason::Aborted))
}

impl EmbeddedSession {
    pub(crate) fn new(host: Arc<HostShared>, runtime: Arc<RuntimeShared>, snapshot: SessionSnapshot) -> Self {
        let inner = SessionInner {
            host,
            runtime,
            state: Mutex::new(snapshot),
            writes: tokio::sync::Mutex::new(()),
            storage_error: Mutex::new(None),
            exec: Mutex::new(ExecState::default()),
            stopping: Mutex::new(SessionStatus::Interrupted),
            approvals: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Vec::new()),
            next_listener: AtomicU64::new(1),
        };
        Self { inner: Arc::new(inner) }
    }

    pub fn id(&self) -> String {
        lock(&self.inner.state).id.clone()
    }

    /// An isolated copy of the current committed state.
    pub fn snapshot(&self) -> SessionSnapshot {
        lock(&self.inner.state).clone()
    }

    /// Subscribe to ordered events. Listeners must return promptly; spawn async commands.
    pub fn subscribe(&self, listener: impl Fn(&SessionEvent) + Send + Sync + 'static) -> Subscription {
        let id = self.inner.next_listener.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.listeners).push((id, Arc::new(listener)));
        Subscription { session: Arc::downgrade(&self.inner), id }
    }

    pub(crate) fn storage_error(&self) -> Option<anyhow::Error> {
        lock(&self.inner.storage_error).as_ref().map(|error| anyhow::anyhow!("{error}"))
    }

    pub(crate) fn abort_controller(&self) {
        if let Some(controller) = &lock(&self.inner.exec).controller {
            controller.cancel();
        }
    }

    pub(crate) fn running(&self) -> Option<Execution> {
        lock(&self.inner.exec).execution.clone()
    }

    pub(crate) async fn change(
        &self,
        mutate: impl FnOnce(&mut SessionSnapshot) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        self.change_with(mutate, |_| None).await
    }

    /// Apply `mutate` to a copy, save it against the current revision, commit, then publish
    /// the payload. Only a failed save ends the session; a failed mutation fails this change.
    pub(crate) async fn change_with(
        &self,
        mutate: impl FnOnce(&mut SessionSnapshot) -> anyhow::Result<()>,
        payload: impl FnOnce(&SessionSnapshot) -> Option<EventPayload>,
    ) -> anyhow::Result<()> {
        let _write = self.inner.writes.lock().await;
        if let Some(error) = self.storage_error() {
            return Err(error);
        }
        let current = lock(&self.inner.state).clone();
        let mut next = current.clone();
        mutate(&mut next)?;
        next.revision += 1;
        let payload = payload(&next);
        if payload.is_some() {
            next.sequence += 1;
        }
        if let Err(error) = self.inner.host.host.store.save(&next, Some(current.revision)).await {
            *lock(&self.inner.storage_error) = Some(error.to_string());
            self.abort_controller();
            return Err(error);
        }
        *lock(&self.inner.state) = next.clone();
        if let Some(payload) = payload {
            let event = SessionEvent { payload, version: 1, session_id: next.id, sequence: next.sequence };
            let listeners: Vec<SessionListener> = lock(&self.inner.listeners).iter().map(|(_, l)| l.clone()).collect();
            for listener in listeners {
                if let Err(panic) = catch_unwind(AssertUnwindSafe(|| listener(&event))) {
                    let message = panic.downcast_ref::<&str>().map(|text| text.to_string());
                    let message = message.or_else(|| panic.downcast_ref::<String>().cloned());
                    self.inner.host.report(message.unwrap_or_else(|| "Session listener panicked".into()));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn assert_open(&self) -> anyhow::Result<()> {
        if lock(&self.inner.state).status == SessionStatus::Disposed {
            anyhow::bail!("Session is disposed");
        }
        match self.storage_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) async fn recover(&self) -> anyhow::Result<()> {
        self.change_with(
            |state| {
                if matches!(state.status, SessionStatus::Running | SessionStatus::AwaitingApproval) {
                    state.status = SessionStatus::Interrupted;
                }
                if let Some(mut partial) = state.partial.take() {
                    partial.stop_reason = StopReason::Aborted;
                    partial.error_message = Some("Host interrupted during generation".into());
                    state.messages.push(Message::Assistant(partial));
                    state.needs_response = true;
                }
                for invocation in &mut state.invocations {
                    match invocation.phase {
                        InvocationPhase::Executing => invocation.phase = InvocationPhase::Uncertain,
                        InvocationPhase::Approval => invocation.phase = InvocationPhase::Pending,
                        _ => {}
                    }
                    if invocation.phase == InvocationPhase::Pending {
                        invocation.approved = None;
                    }
                }
                Ok(())
            },
            |state| Some(EventPayload::Session { status: state.status, error: None }),
        )
        .await?;
        let uncertain: Vec<_> = lock(&self.inner.state)
            .invocations
            .iter()
            .filter(|item| item.phase == InvocationPhase::Uncertain)
            .cloned()
            .collect();
        for invocation in uncertain {
            self.change_with(|_| Ok(()), |_| Some(EventPayload::Recovery { invocation })).await?;
        }
        Ok(())
    }

    fn input(text: &str) -> anyhow::Result<Message> {
        if text.trim().is_empty() {
            anyhow::bail!("Message is empty");
        }
        Ok(Message::User(pi_ai::UserMessage { content: UserContent::Text(text.into()), timestamp: now_ms() }))
    }

    /// Save user input and run until idle or interrupted. Concurrent submissions fail.
    pub fn submit(&self, text: &str) -> BoxFuture<'static, anyhow::Result<()>> {
        let checked = (|| {
            self.assert_open()?;
            if self.running().is_some() {
                anyhow::bail!("Session is running; steer or queue a follow-up");
            }
            let state = lock(&self.inner.state);
            if state.needs_response || state.invocations.iter().any(|item| item.phase != InvocationPhase::Completed) {
                anyhow::bail!("Resume or reconcile the interrupted run first");
            }
            drop(state);
            Self::input(text)
        })();
        match checked {
            Ok(message) => self.start(Some(message)),
            Err(error) => Box::pin(async move { Err(error) }),
        }
    }

    /// Save input for the next turn after the current tool batch.
    pub async fn steer(&self, text: &str) -> anyhow::Result<()> {
        self.queue(text, true).await
    }

    /// Save input to run after current work and steering finish.
    pub async fn follow_up(&self, text: &str) -> anyhow::Result<()> {
        self.queue(text, false).await
    }

    async fn queue(&self, text: &str, steering: bool) -> anyhow::Result<()> {
        self.assert_open()?;
        let message = Self::input(text)?;
        self.change_with(
            |state| {
                if steering {
                    state.steering.push(message)
                } else {
                    state.follow_ups.push(message)
                }
                Ok(())
            },
            |state| {
                Some(EventPayload::Queue { steering: state.steering.clone(), follow_ups: state.follow_ups.clone() })
            },
        )
        .await
    }

    /// Save the model for the next request.
    pub async fn select_model(&self, model: Model) -> anyhow::Result<()> {
        self.assert_open()?;
        self.change(|state| {
            state.model = model;
            Ok(())
        })
        .await
    }

    /// Save the decision and release a pending approval wait.
    pub async fn approve(&self, invocation_id: &str, approved: bool) -> anyhow::Result<()> {
        self.assert_open()?;
        if !lock(&self.inner.approvals).contains_key(invocation_id) {
            anyhow::bail!("Approval is no longer pending");
        }
        self.change_with(
            |state| {
                let invocation = state.invocations.iter_mut().find(|item| item.id == invocation_id);
                let invocation = invocation.ok_or_else(|| anyhow::anyhow!("Approval is no longer pending"))?;
                invocation.approved = Some(approved);
                invocation.phase = InvocationPhase::Pending;
                state.status = SessionStatus::Running;
                Ok(())
            },
            |_| Some(EventPayload::ApprovalDecision { invocation_id: invocation_id.into(), approved }),
        )
        .await?;
        if let Some(waiter) = lock(&self.inner.approvals).remove(invocation_id) {
            let _ = waiter.send(approved);
        }
        Ok(())
    }

    /// Resolve an uncertain invocation from `decision` or the tool's reconciler.
    pub async fn reconcile(&self, invocation_id: &str, decision: Option<Reconciliation>) -> anyhow::Result<()> {
        self.assert_open()?;
        if self.running().is_some() {
            anyhow::bail!("Cancel execution before reconciling");
        }
        let invocation = lock(&self.inner.state).invocations.iter().find(|item| item.id == invocation_id).cloned();
        let Some(invocation) = invocation.filter(|item| item.phase == InvocationPhase::Uncertain) else {
            anyhow::bail!("Invocation is not uncertain");
        };
        let result = match decision {
            Some(decision) => decision,
            None => {
                match self.inner.host.available_tools().into_iter().find(|t| t.declaration().name == invocation.name) {
                    Some(tool) => tool.reconcile(invocation.clone(), CancellationToken::new()).await?,
                    None => Reconciliation::Unknown,
                }
            }
        };
        if result == Reconciliation::Unknown {
            return self.change_with(|_| Ok(()), |_| Some(EventPayload::Recovery { invocation })).await;
        }
        self.change_with(
            |state| {
                let Some(current) = state.invocations.iter_mut().find(|item| item.id == invocation_id) else {
                    anyhow::bail!("Invocation is not uncertain");
                };
                match result {
                    Reconciliation::Completed { result } => {
                        current.phase = InvocationPhase::Completed;
                        current.result = Some(result);
                    }
                    _ => {
                        current.phase = InvocationPhase::Pending;
                        current.result = None;
                        current.approved = None;
                    }
                }
                state.messages.retain(|m| !matches!(m, Message::ToolResult(r) if r.tool_call_id == invocation_id));
                state.needs_response = true;
                state.status = SessionStatus::Paused;
                Ok(())
            },
            |state| {
                let invocation = state.invocations.iter().find(|item| item.id == invocation_id).cloned()?;
                Some(EventPayload::Recovery { invocation })
            },
        )
        .await
    }

    /// Reconcile pending transcript results and continue saved input.
    pub fn resume(&self) -> BoxFuture<'static, anyhow::Result<()>> {
        if let Err(error) = self.assert_open() {
            return Box::pin(async move { Err(error) });
        }
        if let Some(execution) = self.running() {
            return Box::pin(async move { execution.await.map_err(|error| anyhow::anyhow!(error)) });
        }
        if lock(&self.inner.state).invocations.iter().any(|item| item.phase == InvocationPhase::Uncertain) {
            return Box::pin(async { Err(anyhow::anyhow!("Reconcile uncertain tool outcomes before resuming")) });
        }
        self.start(None)
    }

    async fn stop(&self, status: SessionStatus) -> anyhow::Result<()> {
        *lock(&self.inner.stopping) = status;
        self.abort_controller();
        match self.running() {
            Some(execution) => execution.await.map_err(|error| anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }

    /// Abort streaming, approval waits, cooperative tools, and compaction; wait for settlement.
    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.stop(SessionStatus::Interrupted).await
    }

    /// Cancel execution and save a paused checkpoint.
    pub async fn suspend(&self) -> anyhow::Result<()> {
        self.assert_open()?;
        self.stop(SessionStatus::Paused).await?;
        let paused = |state: &mut SessionSnapshot| {
            state.status = SessionStatus::Paused;
            Ok(())
        };
        self.change_with(paused, |_| Some(EventPayload::Session { status: SessionStatus::Paused, error: None })).await
    }

    /// Save terminal disposed state and release the session.
    pub async fn dispose(&self) -> anyhow::Result<()> {
        if lock(&self.inner.state).status == SessionStatus::Disposed {
            return Ok(());
        }
        self.cancel().await?;
        let disposed = |state: &mut SessionSnapshot| {
            state.status = SessionStatus::Disposed;
            Ok(())
        };
        let event = |_: &SessionSnapshot| Some(EventPayload::Session { status: SessionStatus::Disposed, error: None });
        self.change_with(disposed, event).await?;
        lock(&self.inner.listeners).clear();
        lock(&self.inner.runtime.sessions).remove(&self.id());
        Ok(())
    }

    /// Provider context from the context boundary, without failed responses, led by the summary.
    pub(crate) fn context(&self, end: Option<usize>) -> Vec<Message> {
        let state = lock(&self.inner.state);
        let end = end.unwrap_or(state.messages.len());
        let start = state.context_start.min(end);
        let mut messages: Vec<Message> =
            state.messages[start..end].iter().filter(|message| !is_failed(message)).cloned().collect();
        if let Some(summary) = &state.context_summary {
            let system = current_system_prompt(&state.messages);
            let summary = SystemMessage {
                content: UserContent::Text(format!("{system}\nConversation summary:\n{summary}")),
                sections: None,
                tools_added: None,
                tools_removed: None,
                timestamp: now_ms(),
            };
            messages.insert(0, Message::System(summary));
        }
        messages
    }

    /// Register an execution, running `body` as a spawned task whose end clears it.
    pub(crate) fn spawn_execution<F>(&self, body: impl FnOnce(EmbeddedSession, CancellationToken) -> F) -> Execution
    where
        F: std::future::Future<Output = Result<(), String>> + Send + 'static,
    {
        let mut exec = lock(&self.inner.exec);
        let token = CancellationToken::new();
        exec.generation += 1;
        let generation = exec.generation;
        exec.controller = Some(token.clone());
        *lock(&self.inner.stopping) = SessionStatus::Interrupted;
        let task = body(self.clone(), token);
        let session = self.clone();
        let handle = tokio::spawn(async move {
            let outcome = task.await;
            let mut exec = lock(&session.inner.exec);
            if exec.generation == generation {
                exec.execution = None;
                exec.controller = None;
            }
            outcome
        });
        let execution = async move { handle.await.unwrap_or_else(|error| Err(error.to_string())) }.boxed().shared();
        exec.execution = Some(execution.clone());
        execution
    }
}
