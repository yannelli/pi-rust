//! Cache warming and cache-miss notices of a session (upstream `sdk.ts` warmer wiring,
//! `cacheWarmingStatus`, `setCacheWarmingMode`, and the interactive `maybeShowCacheMissNotice`).

use std::sync::{Arc, Mutex, Weak};

use futures::FutureExt;
use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, AssistantMessageEventStream, Context, Message, Model, StopReason, StreamOptions};

use super::{AgentSession, lock};
use crate::cache_stats::{CacheMiss, cache_miss_notice, detect_cache_miss};
use crate::cache_warmer::{
    CacheWarmRequest, CacheWarmer, CacheWarmerHost, CacheWarmingAction, CacheWarmingDecisionEvent, CacheWarmingStatus,
};
use crate::session::EntryData;
use crate::settings::schema::CacheWarmingMode;

/// Extension hook for `cache_warming_decision`: the action to take instead of pi's, or `None`.
pub type CacheWarmingDecider =
    Arc<dyn Fn(CacheWarmingDecisionEvent) -> BoxFuture<'static, Option<CacheWarmingAction>> + Send + Sync>;

pub(crate) struct SessionWarmerHost {
    session: Weak<AgentSession>,
    decider: Mutex<Option<CacheWarmingDecider>>,
}

impl SessionWarmerHost {
    pub fn new(session: Weak<AgentSession>) -> Self {
        Self { session, decider: Mutex::new(None) }
    }
}

impl CacheWarmerHost for SessionWarmerHost {
    fn mode(&self) -> CacheWarmingMode {
        self.session.upgrade().map_or(CacheWarmingMode::Off, |session| session.settings().cache_warming_mode())
    }

    fn last_prompt_tokens(&self) -> u64 {
        let Some(session) = self.session.upgrade() else { return 0 };
        let manager = session.session_manager();
        let branch = manager.get_branch(None);
        let usage = branch.iter().rev().find_map(|entry| match entry.data() {
            EntryData::Message(Message::Assistant(message)) => Some(message.usage),
            _ => None,
        });
        usage.map_or(0, |usage| usage.input + usage.cache_read + usage.cache_write)
    }

    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        match self.session.upgrade() {
            Some(session) => (session.stream_fn())(model, context, options),
            None => pi_ai::error_stream(model, StopReason::Aborted, "Session disposed"),
        }
    }

    fn decide(&self, event: CacheWarmingDecisionEvent) -> BoxFuture<'static, Option<CacheWarmingAction>> {
        let decider = lock(&self.decider).clone();
        match decider {
            Some(decider) => decider(event),
            None => async { None }.boxed(),
        }
    }

    fn warmed(&self, message: &AssistantMessage, note: Option<&str>) {
        let Some(session) = self.session.upgrade() else { return };
        let model = message.response_model.as_deref().unwrap_or(&message.model);
        let appended =
            session.session_manager().append_usage("cache_warm", &message.provider, model, &message.usage, note);
        match appended {
            Ok(id) => session.emit_entry(&id),
            Err(error) => eprintln!("Warning: could not save session: {error:#}"),
        }
    }
}

/// Position and timestamps of a transcript prefix, which survive reprojection of the session.
fn message_stamp(message: &Message) -> (String, i64) {
    let timestamp = match message {
        Message::User(user) => user.timestamp,
        Message::Assistant(assistant) => assistant.timestamp,
        Message::ToolResult(result) => result.timestamp,
        Message::System(system) => system.timestamp,
        Message::Custom(custom) => custom.timestamp(),
    };
    (message.role().to_string(), timestamp)
}

impl AgentSession {
    /// The cache warming state and the decision inputs behind it (`/session`).
    pub fn cache_warming_status(&self) -> CacheWarmingStatus {
        self.cache_warmer.status()
    }

    /// Save the `cacheWarming` mode and reconcile active warming at once.
    pub fn set_cache_warming_mode(&self, mode: CacheWarmingMode) {
        self.settings().set_cache_warming_mode(mode);
        self.cache_warmer.on_mode_changed();
    }

    /// Install the `cache_warming_decision` hook (the extension runner sets it).
    pub fn set_cache_warming_decider(&self, decider: Option<CacheWarmingDecider>) {
        *lock(&self.warmer_host.decider) = decider;
    }

    pub(crate) fn cache_warmer(&self) -> &Arc<CacheWarmer> {
        &self.cache_warmer
    }

    /// Warm the cache entry of a session request while the selection is its model and the
    /// transcript still starts with its messages. A routed request never matches the selection.
    pub(crate) fn start_cache_warming(&self, model: &Model, context: &Context, options: &StreamOptions) {
        let (provider, id) = (model.provider.clone(), model.id.clone());
        let (len, first, last) = self.with_state(|state| {
            (state.messages.len(), state.messages.first().map(message_stamp), state.messages.last().map(message_stamp))
        });
        let weak = self.weak.clone();
        let is_current = Arc::new(move || {
            let Some(session) = weak.upgrade() else { return false };
            session.with_state(|state| {
                state.model.provider == provider
                    && state.model.id == id
                    && state.messages.len() >= len
                    && state.messages.first().map(message_stamp) == first
                    && len.checked_sub(1).and_then(|index| state.messages.get(index)).map(message_stamp) == last
            })
        });
        let request = CacheWarmRequest { model: model.clone(), context: context.clone(), options: options.clone() };
        self.cache_warmer.start(request, is_current);
    }

    /// The cache miss a completed response paid for, with its notice, when `showCacheMissNotices`
    /// is on. Call before the message is persisted.
    pub(crate) fn cache_miss_notice(&self, message: &AssistantMessage) -> Option<(CacheMiss, String)> {
        if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted)
            || !self.settings().show_cache_miss_notices()
        {
            return None;
        }
        let runtime = self.model_runtime().clone();
        let miss = {
            let manager = self.session_manager();
            detect_cache_miss(manager.entries(), message, runtime.as_ref())
        }?;
        cache_miss_notice(&miss).map(|notice| (miss, notice))
    }
}
