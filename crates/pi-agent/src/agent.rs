//! Stateful agent: owns the transcript, queues steering and follow-up input,
//! and reduces loop events into observable state.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt;
use pi_ai::{ContentBlock, Message, Model, ThinkingLevel, UserContent, UserMessage, now_ms};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{
    AfterToolCallFn, AgentContext, AgentLoopConfig, ApiKeyFn, BeforeToolCallFn, EventSink, MessageSource, StreamFn,
    TransformContextFn, run_agent_loop, run_agent_loop_continue,
};
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

/// Hooks and request settings applied to every run.
#[derive(Clone, Default)]
pub struct AgentHooks {
    pub stream_fn: Option<StreamFn>,
    pub api_key: Option<String>,
    pub get_api_key: Option<ApiKeyFn>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    pub transform_context: Option<TransformContextFn>,
    pub tool_execution: ToolExecutionMode,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub session_id: Option<String>,
}

pub type Listener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

struct Inner {
    state: Mutex<AgentState>,
    hooks: Mutex<AgentHooks>,
    steering: Mutex<MessageQueue>,
    follow_up: Mutex<MessageQueue>,
    listeners: Mutex<Vec<(u64, Listener)>>,
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
        lock(&self.inner.listeners).push((id, Arc::new(listener)));
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
        lock(&self.inner.steering).messages.clear();
        lock(&self.inner.follow_up).messages.clear();
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
        let cancel = self.begin_run()?;
        let (mut context, config) = self.snapshot();
        run_agent_loop(messages, &mut context, &config, self.sink(), cancel).await;
        self.finish_run();
        Ok(())
    }

    /// Continue from the transcript; after an assistant message, queued input is used.
    pub async fn continue_run(&self) -> anyhow::Result<()> {
        let last_is_assistant = match lock(&self.inner.state).messages.last() {
            None => anyhow::bail!("No messages to continue from"),
            Some(message) => matches!(message, Message::Assistant(_)),
        };
        if last_is_assistant {
            let steering: Vec<Message> = lock(&self.inner.steering).messages.drain(..).collect();
            let queued = if steering.is_empty() { lock(&self.inner.follow_up).drain() } else { steering };
            if queued.is_empty() {
                anyhow::bail!("Cannot continue from message role: assistant");
            }
            return self.prompt_messages(queued).await;
        }
        let cancel = self.begin_run()?;
        let (mut context, config) = self.snapshot();
        let result = run_agent_loop_continue(&mut context, &config, self.sink(), cancel).await;
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

    fn snapshot(&self) -> (AgentContext, AgentLoopConfig) {
        let state = lock(&self.inner.state).clone();
        let hooks = lock(&self.inner.hooks).clone();
        let context = AgentContext { system_prompt: state.system_prompt, messages: state.messages, tools: state.tools };
        let steering = self.inner.clone();
        let follow_up = self.inner.clone();
        let get_steering_messages: MessageSource = Arc::new(move || {
            let messages = lock(&steering.steering).drain();
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
        };
        (context, config)
    }

    fn sink(&self) -> EventSink {
        let inner = self.inner.clone();
        Arc::new(move |event| {
            {
                let mut state = lock(&inner.state);
                match &event {
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
                    AgentEvent::TurnEnd { message: Message::Assistant(message), .. }
                        if message.error_message.is_some() =>
                    {
                        state.error_message = message.error_message.clone();
                    }
                    AgentEvent::AgentEnd { .. } => state.streaming_message = None,
                    _ => {}
                }
            }
            let listeners: Vec<Listener> =
                lock(&inner.listeners).iter().map(|(_, listener)| listener.clone()).collect();
            for listener in listeners {
                listener(&event);
            }
        })
    }
}
