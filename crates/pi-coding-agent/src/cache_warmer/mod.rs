//! Cache warming (upstream `core/cache-warmer.ts`): keep one prompt cache entry alive by sending
//! its request again with a one-token output cap before the entry expires. `start` replaces the
//! run; refreshes never extend the fixed safety windows.

mod decision;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, AssistantMessageEventStream, Context, Model, StopReason, StreamOptions};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub use self::decision::{
    CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS, CacheWarmingAction, CacheWarmingDecision, CacheWarmingDecisionEvent,
    CacheWarmingPhase, CacheWarmingState, CacheWarmingStatus, IDLE_CONTINUATION_PROBABILITY, MAX_IDLE_WARMING_AGE,
    MAX_WARMING_AGE, cache_warming_delay, evaluate, format_cache_warming_status, is_replayable, prompt_cache_ttl,
};
pub use crate::cache_stats::format_cache_warming_usage;
pub use crate::settings::schema::CacheWarmingMode;

/// What the warmer needs from its session.
pub trait CacheWarmerHost: Send + Sync {
    fn mode(&self) -> CacheWarmingMode;
    /// Prompt size of the latest real request on the branch, as the provider reported it.
    fn last_prompt_tokens(&self) -> u64;
    fn stream(&self, model: &Model, context: &Context, options: StreamOptions) -> AssistantMessageEventStream;
    /// The `cache_warming_decision` hook; `None` keeps pi's decision.
    fn decide(&self, event: CacheWarmingDecisionEvent) -> BoxFuture<'static, Option<CacheWarmingAction>>;
    /// Record a successful refresh as a `cache_warm` usage entry.
    fn warmed(&self, message: &AssistantMessage, note: Option<&str>);
}

/// The request whose cache entry is kept warm, exactly as it was sent.
#[derive(Clone)]
pub struct CacheWarmRequest {
    pub model: Model,
    pub context: Context,
    pub options: StreamOptions,
}

/// False once the session's model or messages no longer match the request.
pub type IsCurrentFn = Arc<dyn Fn() -> bool + Send + Sync>;

struct ActiveRun {
    id: u64,
    request: CacheWarmRequest,
    is_current: IsCurrentFn,
    ttl: Duration,
    delay: Duration,
    /// Latest safe time to send the refresh: half the planned expiry margin is left.
    refresh_deadline: Instant,
    started_at: Instant,
    cancel: CancellationToken,
    phase: CacheWarmingPhase,
    next_warm_at: Instant,
    next_warm_at_ms: i64,
    extension_override: bool,
    /// `None` while a refresh is in flight.
    timer: Option<JoinHandle<()>>,
}

struct Inner {
    run: Option<ActiveRun>,
    inactive: CacheWarmingStatus,
    next_id: u64,
}

pub struct CacheWarmer {
    host: Arc<dyn CacheWarmerHost>,
    inner: Mutex<Inner>,
    weak: Weak<CacheWarmer>,
}

impl CacheWarmer {
    pub fn new(host: Arc<dyn CacheWarmerHost>) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            host,
            inner: Mutex::new(Inner {
                run: None,
                inactive: CacheWarmingStatus::inactive("waiting for first request"),
                next_id: 1,
            }),
            weak: weak.clone(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current state and the decision inputs that produced it.
    pub fn status(&self) -> CacheWarmingStatus {
        if self.host.mode() == CacheWarmingMode::Off {
            return CacheWarmingStatus::inactive("cache warming disabled");
        }
        let snapshot = {
            let inner = self.lock();
            match &inner.run {
                None => return inner.inactive.clone(),
                Some(run) => (
                    run.is_current.clone(),
                    run.request.model.clone(),
                    run.phase,
                    run.timer.is_none(),
                    run.next_warm_at_ms,
                    run.extension_override,
                ),
            }
        };
        let (is_current, model, phase, refreshing, next_warm_at, extension_override) = snapshot;
        if !is_current() {
            return CacheWarmingStatus::inactive("conversation context changed");
        }
        let decision = evaluate(&model, self.host.last_prompt_tokens(), phase);
        if !decision.economics_available && !refreshing {
            return CacheWarmingStatus::inactive("cache economics unavailable");
        }
        CacheWarmingStatus {
            state: if refreshing { CacheWarmingState::Refreshing } else { CacheWarmingState::Scheduled },
            reason: None,
            next_warm_at: Some(next_warm_at),
            decision: Some(decision),
            extension_override,
        }
    }

    /// Keep the cache entry `request` writes warm while `is_current` holds.
    pub fn start(&self, request: CacheWarmRequest, is_current: IsCurrentFn) {
        let mut inner = self.lock();
        clear(&mut inner);
        if self.host.mode() == CacheWarmingMode::Off {
            return stop(&mut inner, "cache warming disabled", None);
        }
        if !is_replayable(&request.model, &request.options) {
            return stop(&mut inner, "request cannot be replayed safely", None);
        }
        let Some(ttl) = prompt_cache_ttl(&request.model, &request.options) else {
            let reason = if request.options.cache_retention == Some(pi_ai::CacheRetention::None) {
                "request disabled prompt caching"
            } else {
                "cache lifetime unavailable"
            };
            return stop(&mut inner, reason, None);
        };
        let Some(delay) = cache_warming_delay(ttl) else {
            return stop(&mut inner, "cache lifetime unavailable", None);
        };
        let id = inner.next_id;
        inner.next_id += 1;
        let now = Instant::now();
        inner.run = Some(ActiveRun {
            id,
            request,
            is_current,
            ttl,
            delay,
            refresh_deadline: now,
            started_at: now,
            cancel: CancellationToken::new(),
            phase: CacheWarmingPhase::Streaming,
            next_warm_at: now,
            next_warm_at_ms: 0,
            extension_override: false,
            timer: None,
        });
        self.schedule(&mut inner);
    }

    /// The agent run settled: `streaming` mode stops, `idle` mode keeps warming for a while.
    pub fn on_agent_settled(&self) {
        let mut inner = self.lock();
        let Some(run) = inner.run.as_mut() else { return };
        if self.host.mode() == CacheWarmingMode::Streaming {
            return stop(&mut inner, "agent run settled", None);
        }
        run.phase = CacheWarmingPhase::Idle;
        let deadline = run.started_at + MAX_IDLE_WARMING_AGE;
        if run.next_warm_at > deadline || Instant::now() >= deadline {
            stop(&mut inner, "30-minute idle safety limit reached", None);
        }
    }

    /// Reconcile the active run after the `cacheWarming` setting changed.
    pub fn on_mode_changed(&self) {
        let mut inner = self.lock();
        let Some(run) = &inner.run else { return };
        if let Some(reason) = self.mode_stop_reason(run.phase) {
            stop(&mut inner, reason, None);
        }
    }

    pub fn cancel(&self) {
        stop(&mut self.lock(), "inactive", None);
    }

    fn mode_stop_reason(&self, phase: CacheWarmingPhase) -> Option<&'static str> {
        match self.host.mode() {
            CacheWarmingMode::Off => Some("cache warming disabled"),
            CacheWarmingMode::Streaming if phase == CacheWarmingPhase::Idle => Some("agent run settled"),
            _ => None,
        }
    }

    fn schedule(&self, inner: &mut Inner) {
        let Some(run) = inner.run.as_mut() else { return };
        run.extension_override = false;
        let now = Instant::now();
        run.next_warm_at = now + run.delay;
        run.next_warm_at_ms = pi_ai::now_ms() + run.delay.as_millis() as i64;
        // A late timer (sleep, a blocked runtime) keeps half the planned margin for dispatch;
        // a later refresh would likely be a full-price cache write.
        run.refresh_deadline = run.next_warm_at + (run.ttl - run.delay) / 2;
        let (max_age, reason) = match run.phase {
            CacheWarmingPhase::Idle => (MAX_IDLE_WARMING_AGE, "30-minute idle safety limit reached"),
            CacheWarmingPhase::Streaming => (MAX_WARMING_AGE, "one-hour safety limit reached"),
        };
        let deadline = run.started_at + max_age;
        if run.next_warm_at > deadline || now >= deadline {
            return stop(inner, reason, None);
        }
        let (weak, id, at) = (self.weak.clone(), run.id, run.next_warm_at);
        run.timer = Some(tokio::spawn(async move {
            tokio::time::sleep_until(at).await;
            if let Some(warmer) = weak.upgrade() {
                warmer.refresh(id).await;
            }
        }));
    }

    /// Whether run `id` is still active and valid; an invalid run stops.
    fn validate(&self, id: u64) -> bool {
        let (phase, is_current) = {
            let inner = self.lock();
            match &inner.run {
                Some(run) if run.id == id => (run.phase, run.is_current.clone()),
                _ => return false,
            }
        };
        let reason = self.mode_stop_reason(phase).or_else(|| (!is_current()).then_some("conversation context changed"));
        let Some(reason) = reason else { return true };
        let mut inner = self.lock();
        if inner.run.as_ref().is_some_and(|run| run.id == id) {
            stop(&mut inner, reason, None);
        }
        false
    }

    fn deadline_missed(&self, id: u64) -> bool {
        let mut inner = self.lock();
        let Some(run) = inner.run.as_ref().filter(|run| run.id == id) else { return true };
        if Instant::now() <= run.refresh_deadline {
            return false;
        }
        stop(&mut inner, "cache refresh deadline missed", None);
        true
    }

    /// The timer fired for run `id`: decide, send the refresh, record it, and schedule again.
    pub(crate) async fn refresh(&self, id: u64) {
        let model = {
            let mut inner = self.lock();
            let Some(run) = inner.run.as_mut().filter(|run| run.id == id) else { return };
            run.timer = None;
            run.request.model.clone()
        };
        if !self.validate(id) || self.deadline_missed(id) {
            return;
        }
        let phase = self.lock().run.as_ref().map_or(CacheWarmingPhase::Streaming, |run| run.phase);
        let decision = evaluate(&model, self.host.last_prompt_tokens(), phase);
        let event = CacheWarmingDecisionEvent {
            warm_cost: decision.warm_cost,
            miss_cost: decision.miss_cost,
            continuation_probability: decision.continuation_probability,
            action: decision.action,
        };
        let action = self.host.decide(event).await.unwrap_or(decision.action);
        if !self.validate(id) || self.deadline_missed(id) {
            return;
        }
        let extension_override = action != decision.action;
        if action == CacheWarmingAction::Stop {
            let reason = match (extension_override, decision.economics_available) {
                (true, _) => "stopped by extension",
                (false, true) => "expected savings below threshold",
                (false, false) => "cache economics unavailable",
            };
            return stop(&mut self.lock(), reason, Some((decision, extension_override)));
        }
        let (request, cancel) = {
            let mut inner = self.lock();
            let Some(run) = inner.run.as_mut().filter(|run| run.id == id) else { return };
            run.extension_override = extension_override;
            (run.request.clone(), run.cancel.clone())
        };
        let mut options = request.options.clone();
        options.max_tokens = Some(1);
        options.max_retries = Some(0);
        options.cancel = cancel;
        // Warming is best effort and never affects the active agent run.
        let message = self.host.stream(&request.model, &request.context, options).result(&request.model).await;
        if self.validate(id) && !matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
            self.host.warmed(&message, extension_override.then_some("extension override"));
        }
        let mut inner = self.lock();
        if inner.run.as_ref().is_some_and(|run| run.id == id) {
            self.schedule(&mut inner);
        }
    }
}

/// End the active run: its timer stops and its request is cancelled.
fn clear(inner: &mut Inner) {
    if let Some(run) = inner.run.take() {
        if let Some(timer) = run.timer {
            timer.abort();
        }
        run.cancel.cancel();
    }
}

fn stop(inner: &mut Inner, reason: &str, stopped: Option<(CacheWarmingDecision, bool)>) {
    clear(inner);
    let mut status = CacheWarmingStatus::inactive(reason);
    if let Some((decision, extension_override)) = stopped {
        status.decision = Some(decision);
        status.extension_override = extension_override;
    }
    inner.inactive = status;
}
