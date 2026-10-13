//! Cache warming economics, eligibility, and status text (upstream `core/cache-warmer.ts`).

use std::time::Duration;

use pi_ai::simple_options::resolve_cache_retention;
use pi_ai::{CacheRetention, Model, StreamOptions, Usage};
use serde_json::{Value, json};

/// Streaming warming never continues past this long after the real request that started it.
pub const MAX_WARMING_AGE: Duration = Duration::from_secs(60 * 60);
/// Idle warming stops sooner: continuation estimates get less reliable with age.
pub const MAX_IDLE_WARMING_AGE: Duration = Duration::from_secs(30 * 60);
/// A refresh is sent only when it is expected to save at least this many dollars.
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// Chance that a real request arrives before the entry expires while the agent is idle.
pub const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

/// Refresh at 90% of the TTL while keeping at least ten seconds of margin.
pub fn cache_warming_delay(ttl: Duration) -> Option<Duration> {
    let ttl_ms = ttl.as_millis() as u64;
    if ttl_ms <= 10_000 {
        return None;
    }
    let delay = ((ttl_ms as f64 * 0.9).min((ttl_ms - 10_000) as f64)).floor() as u64;
    Some(Duration::from_millis(delay.max(1)))
}

/// Lifetime of the cache entry a request writes: the model's `promptCache` tier for the
/// retention the request used. `None` without that tier or with caching off.
pub fn prompt_cache_ttl(model: &Model, options: &StreamOptions) -> Option<Duration> {
    let retention = match resolve_cache_retention(options.cache_retention, &options.env) {
        CacheRetention::None => return None,
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    };
    let seconds = *model.prompt_cache.as_ref()?.get(retention)?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// Whether replaying the request with a one-token cap keeps its cache entry. Anthropic's
/// budget-based thinking derives the budget from `max_tokens`, which keys the cache.
pub fn is_replayable(model: &Model, options: &StreamOptions) -> bool {
    if options.reasoning.is_none() || model.api != "anthropic-messages" {
        return true;
    }
    model.compat.as_ref().and_then(|compat| compat.get("forceAdaptiveThinking")).and_then(Value::as_bool) == Some(true)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheWarmingAction {
    Warm,
    Stop,
}

impl CacheWarmingAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Stop => "stop",
        }
    }
}

/// `streaming` while the agent run that sent the request is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheWarmingPhase {
    Streaming,
    Idle,
}

/// Inputs and outcome of one warm-or-stop decision, as `/session` shows them.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheWarmingDecision {
    pub phase: CacheWarmingPhase,
    /// Price of the refresh: a cache read of the prompt plus one output token.
    pub warm_cost: f64,
    /// Extra price of the next real request if the cache entry is lost.
    pub miss_cost: f64,
    pub continuation_probability: f64,
    /// `continuation_probability * miss_cost - warm_cost`.
    pub expected_savings: f64,
    /// False when the prompt size or the model's prices are unknown.
    pub economics_available: bool,
    /// `warm` when the expected savings are at least $0.05.
    pub action: CacheWarmingAction,
}

/// The `cache_warming_decision` extension event, fired before each refresh with pi's decision.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheWarmingDecisionEvent {
    pub warm_cost: f64,
    pub miss_cost: f64,
    pub continuation_probability: f64,
    pub action: CacheWarmingAction,
}

impl CacheWarmingDecisionEvent {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "cache_warming_decision",
            "warmCost": self.warm_cost,
            "missCost": self.miss_cost,
            "continuationProbability": self.continuation_probability,
            "action": self.action.as_str(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheWarmingState {
    Inactive,
    /// A refresh timer is armed.
    Scheduled,
    /// A warm request is in flight.
    Refreshing,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CacheWarmingStatus {
    pub state: CacheWarmingState,
    /// Why nothing is scheduled.
    pub reason: Option<String>,
    /// Unix milliseconds of the next decision.
    pub next_warm_at: Option<i64>,
    /// The pending decision, or the decision that stopped warming.
    pub decision: Option<CacheWarmingDecision>,
    /// An extension changed `decision.action`.
    pub extension_override: bool,
}

impl CacheWarmingStatus {
    pub fn inactive(reason: &str) -> Self {
        Self {
            state: CacheWarmingState::Inactive,
            reason: Some(reason.into()),
            next_warm_at: None,
            decision: None,
            extension_override: false,
        }
    }
}

fn price(model: &Model, mut usage: Usage) -> f64 {
    usage.apply_cost(&model.cost);
    usage.cost.total
}

/// Pi's decision for a prompt of `prompt_tokens` on `model`.
pub fn evaluate(model: &Model, prompt_tokens: u64, phase: CacheWarmingPhase) -> CacheWarmingDecision {
    let hit = price(model, Usage { cache_read: prompt_tokens, ..Usage::default() });
    let miss = if model.cost.cache_write > 0.0 {
        price(model, Usage { cache_write: prompt_tokens, ..Usage::default() })
    } else {
        price(model, Usage { input: prompt_tokens, ..Usage::default() })
    };
    let warm_cost = price(model, Usage { cache_read: prompt_tokens, output: 1, ..Usage::default() });
    let miss_cost = (miss - hit).max(0.0);
    let continuation_probability = match phase {
        CacheWarmingPhase::Idle => IDLE_CONTINUATION_PROBABILITY,
        CacheWarmingPhase::Streaming => 1.0,
    };
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    let warm = expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS;
    CacheWarmingDecision {
        phase,
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available: prompt_tokens > 0 && (hit > 0.0 || miss > 0.0),
        action: if warm { CacheWarmingAction::Warm } else { CacheWarmingAction::Stop },
    }
}

fn format_dollars(value: f64) -> String {
    if value < 0.0 { format!("-${:.3}", value.abs()) } else { format!("${value:.3}") }
}

fn format_economics(decision: &CacheWarmingDecision) -> String {
    if !decision.economics_available {
        return "cache economics unavailable".into();
    }
    let probability = (decision.continuation_probability * 100.0).round();
    let probability = match decision.phase {
        CacheWarmingPhase::Streaming => format!("{probability}% continuation probability while agent is running"),
        CacheWarmingPhase::Idle => format!("{probability}% continuation probability"),
    };
    let comparison = if decision.action == CacheWarmingAction::Warm { ">=" } else { "<" };
    format!(
        "{probability}, expected savings {} {comparison} ${CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS:.3}",
        format_dollars(decision.expected_savings)
    )
}

fn format_decision_time(next_warm_at: Option<i64>, now_ms: i64) -> String {
    let Some(next) = next_warm_at.filter(|next| *next > now_ms) else { return "Decision now".into() };
    let mut remaining = (next - now_ms + 999) / 1000;
    let hours = remaining / 3600;
    remaining %= 3600;
    let (minutes, seconds) = (remaining / 60, remaining % 60);
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes}m"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(format!("{seconds}s"));
    }
    format!("Decision in {}", parts.join(" "))
}

/// The one-line status `/session` shows.
pub fn format_cache_warming_status(status: &CacheWarmingStatus, now_ms: i64) -> String {
    let decision = status.decision.as_ref().filter(|decision| {
        status.state != CacheWarmingState::Inactive || decision.economics_available || status.extension_override
    });
    let Some(decision) = decision else {
        return format!("Inactive ({})", status.reason.as_deref().unwrap_or("unknown reason"));
    };
    let details = if status.extension_override {
        format!("extension override, {}", format_economics(decision))
    } else {
        format!("{} -> {}", format_economics(decision), decision.action.as_str())
    };
    match status.state {
        CacheWarmingState::Inactive => format!("Stopped ({details})"),
        CacheWarmingState::Refreshing => format!("Warming cache ({details})"),
        CacheWarmingState::Scheduled => format!("{} ({details})", format_decision_time(status.next_warm_at, now_ms)),
    }
}
