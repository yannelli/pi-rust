//! Session events cross from agent threads to the UI thread here: the session listener turns each
//! borrowed [`AgentSessionEvent`] into an owned [`UiEvent`], queues it, and wakes the UI loop.

use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use pi_agent::{AgentEvent, AgentToolResult};
use pi_ai::Message;
use pi_tui::TuiWaker;
use serde_json::Value;

use super::mode::InteractiveMode;
use crate::agent_session::{AgentSession, AgentSessionEvent, CompactionReason, SummarizationSource};
use crate::compaction::CompactionResult;

/// Kept for every session: work for the UI thread outlives session switches.
const ANY_GENERATION: u64 = u64::MAX;

/// Work for the UI thread, sent from any thread.
pub struct UiCall(pub UiCallFn);

/// The work of a [`UiCall`].
pub type UiCallFn = Box<dyn FnOnce(&Rc<InteractiveMode>) + Send>;

/// An owned session event, or a notice from a background task.
pub enum UiEvent {
    AgentStart,
    TurnStart,
    MessageStart(Message),
    MessageUpdate(Message),
    MessageEnd(Message),
    ToolStart {
        id: String,
        name: String,
        args: Value,
        nested: bool,
    },
    ToolUpdate {
        id: String,
        partial: AgentToolResult,
    },
    ToolEnd {
        id: String,
        result: AgentToolResult,
        is_error: bool,
        duration_ms: Option<u64>,
    },
    AgentEnd,
    AgentSettled {
        aborted: bool,
    },
    QueueUpdate,
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        reason: CompactionReason,
        result: Option<CompactionResult>,
        aborted: bool,
        will_retry: bool,
        error_message: Option<String>,
    },
    EntryAppended(Value),
    SessionInfoChanged,
    ThinkingLevelChanged,
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    AutoRetryEnd {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
    SummarizationRetryScheduled {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    SummarizationRetryAttemptStart {
        source: SummarizationSource,
    },
    SummarizationRetryFinished,
    /// The runtime replaced the session (`/new`, `/resume`, fork, import).
    Rebind(Arc<AgentSession>),
    /// A prompt or command failed outside the event stream.
    Error(String),
    /// The theme file changed on disk.
    ThemeChanged,
    /// A virtual model routed a request; the footer shows the route.
    ModelRouted,
    /// The notice of a response that re-billed cached prompt tokens.
    CacheMiss(String),
    /// A notice from a session feature, such as MCP (upstream `ctx.ui.notify`).
    Notify {
        message: String,
        level: pi_mcp::NotifyLevel,
    },
    Call(UiCall),
}

impl UiEvent {
    /// The owned form of a session event; `None` for events the UI ignores.
    pub fn from_session(event: &AgentSessionEvent<'_>) -> Option<UiEvent> {
        Some(match event {
            AgentSessionEvent::Agent(event) => return Self::from_agent(event),
            AgentSessionEvent::AgentEnd { .. } => UiEvent::AgentEnd,
            AgentSessionEvent::AgentSettled { aborted } => UiEvent::AgentSettled { aborted: *aborted },
            AgentSessionEvent::QueueUpdate { .. } => UiEvent::QueueUpdate,
            AgentSessionEvent::CompactionStart { reason } => UiEvent::CompactionStart { reason: *reason },
            AgentSessionEvent::CompactionEnd { reason, result, aborted, will_retry, error_message } => {
                UiEvent::CompactionEnd {
                    reason: *reason,
                    result: result.cloned(),
                    aborted: *aborted,
                    will_retry: *will_retry,
                    error_message: error_message.clone(),
                }
            }
            AgentSessionEvent::EntryAppended { entry } => UiEvent::EntryAppended(entry.clone()),
            AgentSessionEvent::SessionInfoChanged { .. } => UiEvent::SessionInfoChanged,
            AgentSessionEvent::ThinkingLevelChanged { .. } => UiEvent::ThinkingLevelChanged,
            AgentSessionEvent::AutoRetryStart { attempt, max_attempts, delay_ms, .. } => {
                UiEvent::AutoRetryStart { attempt: *attempt, max_attempts: *max_attempts, delay_ms: *delay_ms }
            }
            AgentSessionEvent::AutoRetryEnd { success, attempt, final_error } => {
                UiEvent::AutoRetryEnd { success: *success, attempt: *attempt, final_error: final_error.clone() }
            }
            AgentSessionEvent::SummarizationRetryScheduled { attempt, max_attempts, delay_ms, error_message } => {
                UiEvent::SummarizationRetryScheduled {
                    attempt: *attempt,
                    max_attempts: *max_attempts,
                    delay_ms: *delay_ms,
                    error_message: error_message.clone(),
                }
            }
            AgentSessionEvent::SummarizationRetryAttemptStart { source } => {
                UiEvent::SummarizationRetryAttemptStart { source: *source }
            }
            AgentSessionEvent::SummarizationRetryFinished => UiEvent::SummarizationRetryFinished,
            AgentSessionEvent::BashExecutionUpdate { .. } => return None,
            AgentSessionEvent::ModelRouted { .. } => UiEvent::ModelRouted,
            AgentSessionEvent::CacheMiss { notice, .. } => UiEvent::CacheMiss(notice.clone()),
        })
    }

    fn from_agent(event: &AgentEvent) -> Option<UiEvent> {
        Some(match event {
            AgentEvent::AgentStart => UiEvent::AgentStart,
            AgentEvent::TurnStart => UiEvent::TurnStart,
            AgentEvent::MessageStart { message } => UiEvent::MessageStart(message.clone()),
            AgentEvent::MessageUpdate { message, .. } => UiEvent::MessageUpdate(message.clone()),
            AgentEvent::MessageEnd { message } => UiEvent::MessageEnd(message.clone()),
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args, parent_tool_call_id } => {
                UiEvent::ToolStart {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    args: args.clone(),
                    nested: parent_tool_call_id.is_some(),
                }
            }
            AgentEvent::ToolExecutionUpdate { tool_call_id, partial_result, .. } => {
                UiEvent::ToolUpdate { id: tool_call_id.clone(), partial: partial_result.clone() }
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, result, is_error, duration_ms, .. } => UiEvent::ToolEnd {
                id: tool_call_id.clone(),
                result: result.clone(),
                is_error: *is_error,
                duration_ms: *duration_ms,
            },
            AgentEvent::AgentEnd { .. } | AgentEvent::TurnEnd { .. } => return None,
        })
    }
}

#[derive(Default)]
struct State {
    queue: VecDeque<(u64, UiEvent)>,
    generation: u64,
    subscription: Option<(Arc<AgentSession>, u64)>,
    /// The UI loop ended; nothing queued runs any more.
    closed: bool,
}

/// The queue between session listeners and the UI thread. `Send + Sync`.
pub struct EventBridge {
    state: Mutex<State>,
    waker: TuiWaker,
}

impl EventBridge {
    pub fn new(waker: TuiWaker) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(State::default()), waker })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue `event` and wake the UI loop. A message update replaces a queued one right before it.
    pub fn push(&self, event: UiEvent) {
        {
            let mut state = self.lock();
            let generation = state.generation;
            Self::push_locked(&mut state, generation, event);
        }
        self.waker.wake();
    }

    fn push_locked(state: &mut State, generation: u64, event: UiEvent) {
        if let UiEvent::MessageUpdate(_) = &event
            && let Some((queued_generation, UiEvent::MessageUpdate(_))) = state.queue.back()
            && *queued_generation == generation
        {
            state.queue.pop_back();
        }
        state.queue.push_back((generation, event));
    }

    /// Listen to `session` instead of the session listened to so far.
    pub fn subscribe(self: &Arc<Self>, session: &Arc<AgentSession>) {
        let mut state = self.lock();
        if let Some((old, id)) = state.subscription.take() {
            old.unsubscribe(id);
        }
        state.generation += 1;
        let generation = state.generation;
        let bridge: Weak<EventBridge> = Arc::downgrade(self);
        let id = session.subscribe(move |event| {
            let Some(bridge) = bridge.upgrade() else { return };
            let Some(event) = UiEvent::from_session(event) else { return };
            Self::push_locked(&mut bridge.lock(), generation, event);
            bridge.waker.wake();
        });
        state.subscription = Some((session.clone(), id));
    }

    /// Queue a rebind to `session` and listen to it from now on.
    pub fn rebind(self: &Arc<Self>, session: &Arc<AgentSession>) {
        self.subscribe(session);
        self.push(UiEvent::Rebind(session.clone()));
    }

    /// Run `call` on the UI thread with the mode.
    pub fn call(&self, call: impl FnOnce(&Rc<InteractiveMode>) + Send + 'static) {
        self.lock().queue.push_back((ANY_GENERATION, UiEvent::Call(UiCall(Box::new(call)))));
        self.waker.wake();
    }

    /// Run `call` on the UI thread; the future resolves once it ran, or at once after
    /// [`Self::close`].
    pub fn call_and_wait(
        &self,
        call: impl FnOnce(&Rc<InteractiveMode>) + Send + 'static,
    ) -> futures::future::BoxFuture<'static, ()> {
        let (done, finished) = tokio::sync::oneshot::channel::<()>();
        {
            let mut state = self.lock();
            if state.closed {
                return Box::pin(async {});
            }
            let call = move |mode: &Rc<InteractiveMode>| {
                call(mode);
                let _ = done.send(());
            };
            state.queue.push_back((ANY_GENERATION, UiEvent::Call(UiCall(Box::new(call)))));
        }
        self.waker.wake();
        Box::pin(async move {
            let _ = finished.await;
        })
    }

    /// The UI loop ended: drop the queued events, which releases waiting callers.
    pub fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        state.queue.clear();
    }

    pub fn unsubscribe(&self) {
        if let Some((session, id)) = self.lock().subscription.take() {
            session.unsubscribe(id);
        }
    }

    /// Queued events of the current subscription; events of replaced sessions are dropped.
    pub fn drain(&self) -> Vec<UiEvent> {
        let mut state = self.lock();
        let generation = state.generation;
        state
            .queue
            .drain(..)
            .filter(|(queued, _)| *queued == generation || *queued == ANY_GENERATION)
            .map(|(_, event)| event)
            .collect()
    }

    pub fn waker(&self) -> &TuiWaker {
        &self.waker
    }
}
