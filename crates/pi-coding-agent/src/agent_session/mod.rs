//! The agent session (upstream `core/agent-session.ts`): one conversation with its model, tools,
//! system prompt, queues, persistence, auto retry, compaction, tree navigation, and bash
//! execution. Every mode (print, JSON, RPC, interactive) drives a session through this API.

pub mod auth;
pub mod bash;
pub mod builtins;
mod cache;
mod compaction;
pub mod events;
mod hooks;
mod listener;
mod llama_api;
pub mod login;
mod model;
pub mod model_runtime;
mod output_file;
mod prompt;
mod routing;
mod run;
pub mod runtime;
mod runtime_catalog;
mod runtime_virtual;
pub mod sdk;
pub mod services;
mod tool_api;
mod tools;
mod tree;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use pi_agent::{Agent, AgentState, StreamFn};
use pi_ai::{AssistantMessage, Message, Model, ThinkingLevel};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub use self::bash::BashResult;
pub use self::builtins::{BUILTIN_EXTENSIONS, McpSessionOptions, NotifyFn, NotifyLevel};
pub use self::cache::CacheWarmingDecider;
pub use self::events::{AgentSessionEvent, CompactionReason, SummarizationSource, compaction_result_json};
pub use self::llama_api::LlamaProgressFn;
pub use self::model::ModelCycleResult;
pub use self::model_runtime::{ModelRuntime, ModelRuntimeOptions};
pub use self::prompt::{PromptDisposition, PromptOptions, StreamingBehavior};
pub use self::runtime::{AgentSessionRuntime, ForkPosition, ForkResult, RuntimeFactory, RuntimeParts, RuntimeTarget};
pub use self::runtime_catalog::{CachedFileModelsStore, CatalogSetup, LlamaAuth, ProviderAuth, catalog_base_url};
pub use self::runtime_virtual::RouteOptions;
pub use self::sdk::{CreateAgentSessionOptions, CreateAgentSessionResult, NoTools, create_agent_session};
pub use self::services::{
    AgentSessionServices, CreateServicesOptions, DiagnosticLevel, ResourceOptions, RuntimeDiagnostic,
    create_agent_session_services,
};
pub use self::tools::{ToolInfo, ToolSource};
pub use self::tree::{NavigateTreeOptions, NavigateTreeResult, SessionStatsReport};
use crate::model_resolver::ScopedModel;
use crate::session::SessionManager;
use crate::settings::SettingsManager;
use crate::system_prompt::BuildSystemPromptOptions;

/// Receives session events; see [`AgentSession::subscribe`].
pub type SessionListener = Arc<dyn Fn(&AgentSessionEvent<'_>) + Send + Sync>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Mutable run state shared by the prompt loop and the agent event listener.
#[derive(Default)]
pub(crate) struct RunState {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
    pub pending_next_turn: Vec<Message>,
    pub pending_custom: Vec<Message>,
    pub pending_bash: Vec<Message>,
    pub run_active: bool,
    pub abort_requested: bool,
    pub retry_attempt: u32,
    pub retry_sleep: Option<CancellationToken>,
    pub overflow_recovery_attempted: bool,
    /// The latest assistant message of the run and its session entry.
    pub last_assistant: Option<(AssistantMessage, Option<String>)>,
    pub turn_tool_results: Vec<String>,
    pub last_tool_results: Vec<String>,
    pub manual_compaction: Option<CancellationToken>,
    pub auto_compaction: Option<CancellationToken>,
    pub branch_summary: Option<CancellationToken>,
    pub bash: Vec<(u64, CancellationToken)>,
    pub next_bash: u64,
    /// Prompt options of the active run.
    pub run_prompt: Option<BuildSystemPromptOptions>,
    /// The failed response the next request retries (upstream `_failedResponse`).
    pub failed_response: Option<AssistantMessage>,
    /// Why routing the current request failed; its stream ends with this error.
    pub route_failure: Option<String>,
}

/// One conversation. Create it with [`create_agent_session`]; it is always held in an `Arc`.
pub struct AgentSession {
    agent: Agent,
    services: AgentSessionServices,
    session: Mutex<SessionManager>,
    listeners: Mutex<Vec<(u64, SessionListener)>>,
    next_listener: AtomicU64,
    pub(crate) state: Mutex<RunState>,
    idle: watch::Sender<bool>,
    pub(crate) tools: Mutex<tools::ToolState>,
    pub(crate) builtins: Mutex<builtins::BuiltinState>,
    /// The auth-wrapped stream function, shared by session and summarization requests.
    stream_fn: StreamFn,
    /// Whether the session was created with its own stream function (tests, SDK hosts).
    has_custom_stream: bool,
    model_selected: AtomicBool,
    scoped_models: Mutex<Vec<ScopedModel>>,
    agent_listener: Mutex<Option<u64>>,
    disposed: AtomicBool,
    cache_warmer: Arc<crate::cache_warmer::CacheWarmer>,
    warmer_host: Arc<cache::SessionWarmerHost>,
    weak: Weak<AgentSession>,
}

impl AgentSession {
    /// The low-level agent. Its transcript mirrors the session projection.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    pub fn services(&self) -> &AgentSessionServices {
        &self.services
    }

    pub fn settings(&self) -> &Arc<SettingsManager> {
        &self.services.settings
    }

    pub fn model_runtime(&self) -> &Arc<ModelRuntime> {
        &self.services.model_runtime
    }

    pub fn cwd(&self) -> &Path {
        &self.services.cwd
    }

    pub(crate) fn stream_fn(&self) -> &StreamFn {
        &self.stream_fn
    }

    pub(crate) fn has_custom_stream_fn(&self) -> bool {
        self.has_custom_stream
    }

    /// The session tree, locked. Do not hold the guard across session calls.
    pub fn session_manager(&self) -> MutexGuard<'_, SessionManager> {
        lock(&self.session)
    }

    /// A copy of the agent state (messages, model, tools, streaming flags).
    pub fn state(&self) -> AgentState {
        self.agent.state()
    }

    /// Read part of the agent state without copying the transcript.
    pub fn with_state<R>(&self, read: impl FnOnce(&AgentState) -> R) -> R {
        let mut result = None;
        self.agent.update_state(|state| result = Some(read(state)));
        result.expect("update_state runs the closure")
    }

    /// The finalized transcript, including custom roles such as `bashExecution`.
    pub fn messages(&self) -> Vec<Message> {
        self.with_state(|state| state.messages.clone())
    }

    /// The selected model; `None` until one is chosen (for example before `/login`).
    pub fn model(&self) -> Option<Model> {
        self.model_selected.load(Ordering::Relaxed).then(|| self.with_state(|state| state.model.clone()))
    }

    pub fn thinking_level(&self) -> ThinkingLevel {
        self.with_state(|state| state.thinking_level)
    }

    pub fn session_file(&self) -> Option<PathBuf> {
        self.session_manager().file().map(Path::to_path_buf)
    }

    pub fn session_id(&self) -> String {
        self.session_manager().id().to_string()
    }

    pub fn session_name(&self) -> Option<String> {
        self.session_manager().session_name()
    }

    /// Models for cycling, from `--models` or `enabledModels`.
    pub fn scoped_models(&self) -> Vec<ScopedModel> {
        lock(&self.scoped_models).clone()
    }

    pub fn set_scoped_models(&self, scoped: Vec<ScopedModel>) {
        *lock(&self.scoped_models) = scoped;
    }

    /// Whether an agent run or its post-run continuation is active.
    pub fn is_streaming(&self) -> bool {
        lock(&self.state).run_active
    }

    /// Whether compaction or branch summarization is running.
    pub fn is_compacting(&self) -> bool {
        let state = lock(&self.state);
        state.manual_compaction.is_some() || state.auto_compaction.is_some() || state.branch_summary.is_some()
    }

    /// No agent run, compaction, branch summary, or retry is active.
    pub fn is_idle(&self) -> bool {
        let state = lock(&self.state);
        !state.run_active
            && state.manual_compaction.is_none()
            && state.auto_compaction.is_none()
            && state.branch_summary.is_none()
    }

    pub(crate) fn notify_idle(&self) {
        self.idle.send_replace(self.is_idle());
    }

    /// Wait until the session is idle.
    pub async fn wait_for_idle(&self) {
        let mut receiver = self.idle.subscribe();
        if self.is_idle() {
            return;
        }
        let _ = receiver.wait_for(|idle| *idle).await;
    }

    /// Receive session events until [`Self::unsubscribe`]. Listeners run synchronously and
    /// must not block; they may call back into the session.
    pub fn subscribe(&self, listener: impl Fn(&AgentSessionEvent<'_>) + Send + Sync + 'static) -> u64 {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        lock(&self.listeners).push((id, Arc::new(listener)));
        id
    }

    pub fn unsubscribe(&self, id: u64) {
        lock(&self.listeners).retain(|(listener, _)| *listener != id);
    }

    pub(crate) fn emit(&self, event: &AgentSessionEvent<'_>) {
        let listeners: Vec<SessionListener> =
            lock(&self.listeners).iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(event);
        }
    }

    pub(crate) fn emit_entry(&self, entry_id: &str) {
        let entry =
            self.session_manager().get_entry(entry_id).map(|entry| serde_json::Value::Object(entry.raw().clone()));
        if let Some(entry) = entry {
            self.emit(&AgentSessionEvent::EntryAppended { entry });
        }
    }

    /// Set the agent transcript from the session projection (upstream `refreshContext`).
    pub fn refresh_context(&self) {
        let messages = self.session_manager().build_session_context().messages;
        self.agent.update_state(|state| state.messages = messages);
    }

    /// Publish the session, model, and reasoning level to bash commands as `PI_*` variables.
    pub(crate) fn refresh_environment(&self) {
        let mut variables = BTreeMap::new();
        variables.insert("PI_SESSION_ID".to_string(), self.session_id());
        if let Some(file) = self.session_file() {
            variables.insert("PI_SESSION_FILE".to_string(), file.display().to_string());
        }
        if let Some(model) = self.model() {
            variables.insert("PI_PROVIDER".to_string(), model.provider);
            variables.insert("PI_MODEL".to_string(), model.id);
        }
        variables.insert("PI_REASONING_LEVEL".to_string(), self.thinking_level().as_str().to_string());
        crate::tools::set_session_environment(variables);
    }

    /// Abort all work, detach from the agent, and drop every listener.
    pub fn dispose(&self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.request_abort();
        self.abort_bash();
        self.cache_warmer.cancel();
        if let Some(manager) = self.stop_mcp()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move { manager.shutdown().await });
        }
        if let Some(id) = lock(&self.agent_listener).take() {
            self.agent.unsubscribe(id);
        }
        lock(&self.listeners).clear();
    }

    /// Stop the active run, retry, compaction, and branch summary without waiting.
    pub fn request_abort(&self) {
        {
            let mut state = lock(&self.state);
            if state.run_active {
                state.abort_requested = true;
            }
        }
        self.abort_retry();
        self.abort_compaction();
        self.abort_branch_summary();
        self.agent.abort();
    }

    /// Stop the current operation and wait until the session is idle.
    pub async fn abort(&self) {
        self.request_abort();
        self.wait_for_idle().await;
    }

    /// Text of the last assistant message, skipping aborted empty ones.
    pub fn last_assistant_text(&self) -> Option<String> {
        self.with_state(|state| {
            let last = state.messages.iter().rev().find_map(|message| match message {
                Message::Assistant(assistant)
                    if !(assistant.stop_reason == pi_ai::StopReason::Aborted && assistant.content.is_empty()) =>
                {
                    Some(assistant)
                }
                _ => None,
            })?;
            let text: String = last.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect();
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        })
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        if let Some(id) = lock(&self.agent_listener).take() {
            self.agent.unsubscribe(id);
        }
    }
}
