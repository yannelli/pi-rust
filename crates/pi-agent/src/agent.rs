//! Stateful agent: owns the transcript, queues steering and follow-up input,
//! and reduces loop events into observable state.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt;
use futures::future::BoxFuture;
use pi_ai::transcript::{create_initial_system_message, current_system_message, current_system_prompt};
use pi_ai::{ContentBlock, Message, Model, StreamOptions, ThinkingLevel, UserContent, UserMessage, now_ms};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{
    AfterToolCallFn, AgentContext, AgentLoopConfig, ApiKeyFn, BeforeToolCallFn, CallableToolsFn, ConvertToLlmFn,
    EventSink, MessageSource, StreamFn, TransformContextFn, run_agent_loop, run_agent_loop_continue,
};
use crate::hooks::{AgentEventHookFn, FinishTurnFn, PrepareNextTurnFn, PrepareRequestFn};
use crate::types::{AgentEvent, SharedTool, ToolExecutionMode};

/// How queued messages are released to the loop.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

#[derive(Default)]
struct MessageQueue {
    mode: QueueMode,
    messages: VecDeque<Message>,
}

impl MessageQueue {
    fn peek(&self) -> Vec<Message> {
        match self.mode {
            QueueMode::All => self.messages.iter().cloned().collect(),
            QueueMode::OneAtATime => self.messages.front().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<Message> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}

/// Observable agent state.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: Option<String>,
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<SharedTool>,
    pub messages: Vec<Message>,
    pub is_streaming: bool,
    pub streaming_message: Option<Message>,
    pub pending_tool_calls: HashSet<String>,
    pub error_message: Option<String>,
}

impl AgentState {
    /// The system prompt: `system_prompt` when set, else replayed from the transcript.
    pub fn current_system_prompt(&self) -> String {
        match &self.system_prompt {
            Some(prompt) => prompt.clone(),
            None => current_system_prompt(&self.messages),
        }
    }
}

/// Initial state for [`Agent::with_initial_state`]. The prompt and tools become the leading
/// system message unless `messages` already starts with one.
#[derive(Clone, Default)]
pub struct AgentInitialState {
    pub system_prompt: Option<String>,
    pub model: Option<Model>,
    pub thinking_level: ThinkingLevel,
    pub tools: Vec<SharedTool>,
    pub messages: Vec<Message>,
}

/// Hooks and request settings applied to every run.
#[derive(Clone, Default)]
pub struct AgentHooks {
    pub stream_fn: Option<StreamFn>,
    pub api_key: Option<String>,
    pub get_api_key: Option<ApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub transform_context: Option<TransformContextFn>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    pub finish_turn: Option<FinishTurnFn>,
    pub prepare_request: Option<PrepareRequestFn>,
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    pub callable_tools: Option<CallableToolsFn>,
    /// Awaited for every loop event after state is updated and before listeners run, as
    /// upstream awaits its async listeners. For `MessageEnd`, a returned message with the same
    /// role replaces the finalized one in the state, the transcript, and the listener event.
    pub on_event: Option<AgentEventHookFn>,
    /// Base request options such as transport, thinking budgets, payload observers, and identity.
    pub stream_options: StreamOptions,
    pub tool_execution: ToolExecutionMode,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub session_id: Option<String>,
}

pub type Listener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;
/// A listener the run awaits, with the run's abort signal (upstream async `subscribe`).
pub type AsyncListener = Arc<dyn Fn(AgentEvent, CancellationToken) -> BoxFuture<'static, ()> + Send + Sync>;

/// Sync and async listeners share one list so they run in subscription order.
#[derive(Clone)]
enum AnyListener {
    Sync(Listener),
    Async(AsyncListener),
}

struct Inner {
    state: Mutex<AgentState>,
    hooks: Mutex<AgentHooks>,
    steering: Mutex<MessageQueue>,
    follow_up: Mutex<MessageQueue>,
    listeners: Mutex<Vec<(u64, AnyListener)>>,
    next_listener: AtomicU64,
    run: Mutex<Option<CancellationToken>>,
    running: watch::Sender<bool>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cloning shares the same agent.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

impl Agent {
    pub fn new(model: Model) -> Self {
        let state = AgentState {
            system_prompt: None,
            model,
            thinking_level: ThinkingLevel::Off,
            tools: Vec::new(),
            messages: Vec::new(),
            is_streaming: false,
            streaming_message: None,
            pending_tool_calls: HashSet::new(),
            error_message: None,
        };
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                hooks: Mutex::new(AgentHooks::default()),
                steering: Mutex::new(MessageQueue::default()),
                follow_up: Mutex::new(MessageQueue::default()),
                listeners: Mutex::new(Vec::new()),
                next_listener: AtomicU64::new(0),
                run: Mutex::new(None),
                running: watch::channel(false).0,
            }),
        }
    }

    /// An agent whose transcript carries the prompt and tool declarations, as upstream.
    pub fn with_initial_state(initial: AgentInitialState) -> Self {
        let unknown = || Model {
            id: "unknown".into(),
            name: "unknown".into(),
            api: "unknown".into(),
            provider: "unknown".into(),
            ..Model::default()
        };
        let agent = Self::new(initial.model.unwrap_or_else(unknown));
        let declarations: Vec<_> = initial.tools.iter().map(|tool| tool.declaration()).collect();
        let mut messages = initial.messages;
        if !matches!(messages.first(), Some(Message::System(_)))
            && let Some(system) = create_initial_system_message(initial.system_prompt.as_deref(), &declarations)
        {
            messages.insert(0, Message::System(system));
        }
        agent.update_state(|state| {
            state.thinking_level = initial.thinking_level;
            state.tools = initial.tools;
            state.messages = messages;
        });
        agent
    }

    /// Clear the transcript and queues, keeping the replayed prompt and tool baseline.
    pub fn reset(&self) -> anyhow::Result<()> {
        if lock(&self.inner.run).is_some() {
            anyhow::bail!("Agent is already processing. Wait for completion before resetting.");
        }
        {
            let mut state = lock(&self.inner.state);
            let baseline = current_system_message(&state.messages);
            state.messages = baseline.map(Message::System).into_iter().collect();
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
            state.error_message = None;
        }
        self.clear_queues();
        Ok(())
    }

    /// A copy of the current state.
    pub fn state(&self) -> AgentState {
        lock(&self.inner.state).clone()
    }

    /// Change state between runs or from listeners; changes apply to the next request.
    pub fn update_state(&self, update: impl FnOnce(&mut AgentState)) {
        update(&mut lock(&self.inner.state));
    }

    pub fn set_hooks(&self, hooks: AgentHooks) {
        *lock(&self.inner.hooks) = hooks;
    }

    pub fn update_hooks(&self, update: impl FnOnce(&mut AgentHooks)) {
        update(&mut lock(&self.inner.hooks));
    }

    pub fn set_queue_modes(&self, steering: QueueMode, follow_up: QueueMode) {
        lock(&self.inner.steering).mode = steering;
        lock(&self.inner.follow_up).mode = follow_up;
    }

    /// Register a listener; returns an id for `unsubscribe`.
    pub fn subscribe(&self, listener: impl Fn(&AgentEvent) + Send + Sync + 'static) -> u64 {
        let id = self.inner.next_listener.fetch_add(1, Ordering::SeqCst);
        lock(&self.inner.listeners).push((id, AnyListener::Sync(Arc::new(listener))));
        id
    }

    /// Register a listener the loop awaits for each event before it continues, as upstream
    /// `subscribe` awaits async listeners. Listeners run in subscription order and get the
    /// run's abort signal. Returns an id for `unsubscribe`.
    pub fn subscribe_async<F, Fut>(&self, listener: F) -> u64
    where
        F: Fn(AgentEvent, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let id = self.inner.next_listener.fetch_add(1, Ordering::SeqCst);
        let listener: AsyncListener = Arc::new(move |event, signal| listener(event, signal).boxed());
        lock(&self.inner.listeners).push((id, AnyListener::Async(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        lock(&self.inner.listeners).retain(|(listener_id, _)| *listener_id != id);
    }

    /// Queue a message delivered after the current turn's tool calls.
    pub fn steer(&self, message: Message) {
        lock(&self.inner.steering).messages.push_back(message);
    }

    /// Queue a message delivered when the agent would otherwise stop.
    pub fn follow_up(&self, message: Message) {
        lock(&self.inner.follow_up).messages.push_back(message);
    }

    pub fn clear_queues(&self) {
        self.clear_steering_queue();
        self.clear_follow_up_queue();
    }

    pub fn clear_steering_queue(&self) {
        lock(&self.inner.steering).messages.clear();
    }

    pub fn clear_follow_up_queue(&self) {
        lock(&self.inner.follow_up).messages.clear();
    }

    /// The messages the next drain point would take, without consuming them.
    pub fn peek_queued_messages(&self) -> Vec<Message> {
        let steering = lock(&self.inner.steering).peek();
        if steering.is_empty() { lock(&self.inner.follow_up).peek() } else { steering }
    }

    pub fn steering_mode(&self) -> QueueMode {
        lock(&self.inner.steering).mode
    }

    pub fn follow_up_mode(&self) -> QueueMode {
        lock(&self.inner.follow_up).mode
    }

    /// Cancellation token of the active run, if any.
    pub fn signal(&self) -> Option<CancellationToken> {
        lock(&self.inner.run).clone()
    }

    /// Remove and return all queued messages, steering first.
    pub fn take_queued(&self) -> Vec<Message> {
        let mut messages: Vec<Message> = lock(&self.inner.steering).messages.drain(..).collect();
        messages.extend(lock(&self.inner.follow_up).messages.drain(..));
        messages
    }

    pub fn has_queued_messages(&self) -> bool {
        !lock(&self.inner.steering).messages.is_empty() || !lock(&self.inner.follow_up).messages.is_empty()
    }

    pub fn is_running(&self) -> bool {
        *self.inner.running.borrow()
    }

    /// Cancel the active run, if any.
    pub fn abort(&self) {
        if let Some(cancel) = lock(&self.inner.run).as_ref() {
            cancel.cancel();
        }
    }

    /// Wait until the run, including the listeners' handling of `agent_end`, has finished.
    pub async fn wait_for_idle(&self) {
        let mut receiver = self.inner.running.subscribe();
        let _ = receiver.wait_for(|running| !running).await;
    }

    /// Run with a text prompt and optional images.
    pub async fn prompt(&self, text: &str, images: Vec<ContentBlock>) -> anyhow::Result<()> {
        let content = if images.is_empty() {
            UserContent::Text(text.to_string())
        } else {
            let mut blocks = vec![ContentBlock::text(text)];
            blocks.extend(images);
            UserContent::Blocks(blocks)
        };
        self.prompt_messages(vec![Message::User(UserMessage { content, timestamp: now_ms() })]).await
    }

    pub async fn prompt_messages(&self, messages: Vec<Message>) -> anyhow::Result<()> {
        self.run_prompt(messages, false).await
    }

    async fn run_prompt(&self, messages: Vec<Message>, skip_initial_steering_poll: bool) -> anyhow::Result<()> {
        let cancel = self.begin_run()?;
        let (mut context, config) = self.snapshot(skip_initial_steering_poll);
        let sink = self.sink(cancel.clone());
        run_agent_loop(messages, &mut context, &config, sink, cancel).await;
        self.finish_run();
        Ok(())
    }

    /// Continue from the transcript; after an assistant message, queued input is used.
    pub async fn continue_run(&self) -> anyhow::Result<()> {
        let last_is_assistant = {
            let state = lock(&self.inner.state);
            if state.messages.iter().all(|message| matches!(message, Message::System(_))) {
                anyhow::bail!("No messages to continue from");
            }
            matches!(state.messages.last(), Some(Message::Assistant(_)))
        };
        if last_is_assistant {
            // Steering already delivered here is not polled again when the run starts.
            let steering = lock(&self.inner.steering).drain();
            if !steering.is_empty() {
                return self.run_prompt(steering, true).await;
            }
            let follow_ups = lock(&self.inner.follow_up).drain();
            if follow_ups.is_empty() {
                anyhow::bail!("Cannot continue from message role: assistant");
            }
            return self.run_prompt(follow_ups, false).await;
        }
        let cancel = self.begin_run()?;
        let (mut context, config) = self.snapshot(false);
        let sink = self.sink(cancel.clone());
        let result = run_agent_loop_continue(&mut context, &config, sink, cancel).await;
        self.finish_run();
        result.map(|_| ())
    }

    fn begin_run(&self) -> anyhow::Result<CancellationToken> {
        let mut run = lock(&self.inner.run);
        if run.is_some() {
            anyhow::bail!(
                "Agent is already processing. Use steer() or follow_up() to queue messages, or wait for completion."
            );
        }
        let cancel = CancellationToken::new();
        *run = Some(cancel.clone());
        let mut state = lock(&self.inner.state);
        state.is_streaming = true;
        state.streaming_message = None;
        state.error_message = None;
        self.inner.running.send_replace(true);
        Ok(cancel)
    }

    fn finish_run(&self) {
        {
            let mut state = lock(&self.inner.state);
            state.is_streaming = false;
            state.streaming_message = None;
            state.pending_tool_calls.clear();
        }
        *lock(&self.inner.run) = None;
        self.inner.running.send_replace(false);
    }

    fn snapshot(&self, skip_initial_steering_poll: bool) -> (AgentContext, AgentLoopConfig) {
        let state = lock(&self.inner.state).clone();
        let hooks = lock(&self.inner.hooks).clone();
        let context = AgentContext { system_prompt: state.system_prompt, messages: state.messages, tools: state.tools };
        let steering = self.inner.clone();
        let follow_up = self.inner.clone();
        let skip = Arc::new(AtomicBool::new(skip_initial_steering_poll));
        let get_steering_messages: MessageSource = Arc::new(move || {
            let messages =
                if skip.swap(false, Ordering::SeqCst) { Vec::new() } else { lock(&steering.steering).drain() };
            async move { messages }.boxed()
        });
        let get_follow_up_messages: MessageSource = Arc::new(move || {
            let messages = lock(&follow_up.follow_up).drain();
            async move { messages }.boxed()
        });
        let config = AgentLoopConfig {
            model: state.model,
            reasoning: (state.thinking_level != ThinkingLevel::Off).then_some(state.thinking_level),
            stream_fn: hooks.stream_fn,
            api_key: hooks.api_key,
            get_api_key: hooks.get_api_key,
            max_tokens: hooks.max_tokens,
            temperature: hooks.temperature,
            session_id: hooks.session_id,
            tool_execution: hooks.tool_execution,
            get_steering_messages: Some(get_steering_messages),
            get_follow_up_messages: Some(get_follow_up_messages),
            before_tool_call: hooks.before_tool_call,
            after_tool_call: hooks.after_tool_call,
            transform_context: hooks.transform_context,
            convert_to_llm: hooks.convert_to_llm,
            finish_turn: hooks.finish_turn,
            prepare_request: hooks.prepare_request,
            prepare_next_turn: hooks.prepare_next_turn,
            callable_tools: hooks.callable_tools,
            stream_options: hooks.stream_options,
        };
        (context, config)
    }

    /// The run's event sink: reduce state, await the `on_event` hook, then await each listener.
    fn sink(&self, cancel: CancellationToken) -> EventSink {
        let inner = self.inner.clone();
        let hook = lock(&self.inner.hooks).on_event.clone();
        let sink: EventSink = Arc::new(move |event| {
            let (inner, hook, cancel) = (inner.clone(), hook.clone(), cancel.clone());
            async move {
                reduce(&inner, &event);
                let replacement = match &hook {
                    Some(hook) => hook(event.clone(), cancel.clone()).await,
                    None => None,
                };
                let event = match (event, replacement) {
                    (AgentEvent::MessageEnd { message }, Some(replaced)) if replaced.role() == message.role() => {
                        if let Some(last) = lock(&inner.state).messages.last_mut() {
                            *last = replaced.clone();
                        }
                        AgentEvent::MessageEnd { message: replaced }
                    }
                    (event, _) => event,
                };
                let listeners: Vec<AnyListener> =
                    lock(&inner.listeners).iter().map(|(_, listener)| listener.clone()).collect();
                for listener in listeners {
                    match listener {
                        AnyListener::Sync(listener) => listener(&event),
                        AnyListener::Async(listener) => listener(event.clone(), cancel.clone()).await,
                    }
                }
                match event {
                    AgentEvent::MessageEnd { message } => Some(message),
                    _ => None,
                }
            }
            .boxed()
        });
        sink
    }
}

/// Reduce a loop event into the observable state before hooks and listeners see it.
fn reduce(inner: &Inner, event: &AgentEvent) {
    let mut state = lock(&inner.state);
    match event {
        AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
            state.streaming_message = Some(message.clone());
        }
        AgentEvent::MessageEnd { message } => {
            state.streaming_message = None;
            state.messages.push(message.clone());
        }
        AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
            state.pending_tool_calls.insert(tool_call_id.clone());
        }
        AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
            state.pending_tool_calls.remove(tool_call_id);
        }
        AgentEvent::TurnEnd { message: Message::Assistant(message), .. } if message.error_message.is_some() => {
            state.error_message = message.error_message.clone();
        }
        AgentEvent::AgentEnd { .. } => state.streaming_message = None,
        _ => {}
    }
}
