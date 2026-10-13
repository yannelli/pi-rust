//! Port of upstream `test/cache-warmer.test.ts`, on a paused tokio clock.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::FutureExt;
use pi_ai::{CacheRetention, ModelCost, ThinkingLevel, Usage, error_stream, event_channel};
use serde_json::json;
use tokio::sync::oneshot;

use super::*;

fn model(api: &str, cost: ModelCost, prompt_cache: Option<(f64, f64)>) -> Model {
    Model {
        id: "m".into(),
        name: "m".into(),
        api: api.into(),
        provider: "p".into(),
        cost,
        prompt_cache: prompt_cache
            .map(|(short, long)| BTreeMap::from([("short".into(), short), ("long".into(), long)])),
        ..Model::default()
    }
}

/// Prices of an Anthropic model with adaptive thinking.
fn adaptive() -> Model {
    let cost = ModelCost { input: 5.0, output: 25.0, cache_read: 0.5, cache_write: 6.25, ..Default::default() };
    let mut model = model("anthropic-messages", cost, Some((300.0, 3600.0)));
    model.compat = Some(json!({"forceAdaptiveThinking": true}));
    model
}

fn budget() -> Model {
    Model { compat: None, ..adaptive() }
}

fn warm_usage() -> Usage {
    let mut usage = Usage { output: 1, cache_read: 100, total_tokens: 101, ..Usage::default() };
    usage.cost.cache_read = 0.01;
    usage.cost.total = 0.01;
    usage
}

type Reply = Box<dyn Fn(&Model) -> AssistantMessageEventStream + Send + Sync>;

struct FakeHost {
    mode: Mutex<CacheWarmingMode>,
    prompt_tokens: AtomicU64,
    calls: Mutex<Vec<StreamOptions>>,
    events: Mutex<Vec<CacheWarmingDecisionEvent>>,
    warmed: Mutex<Vec<(u64, Option<String>)>>,
    decide: Option<CacheWarmingAction>,
    reply: Reply,
}

fn done(model: &Model, stop_reason: StopReason) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut message = AssistantMessage::empty(model);
    message.usage = warm_usage();
    message.stop_reason = stop_reason;
    sender.push(pi_ai::AssistantMessageEvent::Done { reason: stop_reason, message });
    stream
}

fn host(mode: CacheWarmingMode, prompt_tokens: u64, decide: Option<CacheWarmingAction>, reply: Reply) -> Arc<FakeHost> {
    Arc::new(FakeHost {
        mode: Mutex::new(mode),
        prompt_tokens: AtomicU64::new(prompt_tokens),
        calls: Mutex::default(),
        events: Mutex::default(),
        warmed: Mutex::default(),
        decide,
        reply,
    })
}

fn ok_reply() -> Reply {
    Box::new(|model| done(model, StopReason::Length))
}

impl CacheWarmerHost for FakeHost {
    fn mode(&self) -> CacheWarmingMode {
        *self.mode.lock().unwrap()
    }

    fn last_prompt_tokens(&self) -> u64 {
        self.prompt_tokens.load(Ordering::SeqCst)
    }

    fn stream(&self, model: &Model, _context: &Context, options: StreamOptions) -> AssistantMessageEventStream {
        self.calls.lock().unwrap().push(options);
        (self.reply)(model)
    }

    fn decide(&self, event: CacheWarmingDecisionEvent) -> BoxFuture<'static, Option<CacheWarmingAction>> {
        self.events.lock().unwrap().push(event);
        let decision = self.decide;
        async move { decision }.boxed()
    }

    fn warmed(&self, message: &AssistantMessage, note: Option<&str>) {
        self.warmed.lock().unwrap().push((message.usage.cache_read, note.map(String::from)));
    }
}

fn request(model: Model, options: StreamOptions) -> CacheWarmRequest {
    CacheWarmRequest { model, context: Context::default(), options }
}

fn current() -> IsCurrentFn {
    Arc::new(|| true)
}

async fn advance(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[test]
fn derives_eligibility_and_timing_from_retention() {
    let options = |retention: Option<CacheRetention>, env: Option<&str>| StreamOptions {
        cache_retention: retention,
        env: env
            .map(|value| BTreeMap::from([("PI_CACHE_RETENTION".to_string(), value.to_string())]))
            .unwrap_or_default(),
        ..StreamOptions::default()
    };
    let unknown = Model { prompt_cache: None, ..adaptive() };
    let openai = model("openai-responses", ModelCost::default(), Some((300.0, 86_400.0)));
    let ttl = |model: &Model, options: StreamOptions| prompt_cache_ttl(model, &options).map(|ttl| ttl.as_millis());
    assert_eq!(
        [
            ttl(&adaptive(), options(None, None)),
            ttl(&adaptive(), options(Some(CacheRetention::Long), None)),
            ttl(&adaptive(), options(Some(CacheRetention::None), None)),
            ttl(&adaptive(), options(None, Some("long"))),
            ttl(&openai, options(Some(CacheRetention::Long), None)),
            ttl(&unknown, options(None, None)),
        ],
        [Some(300_000), Some(3_600_000), None, Some(3_600_000), Some(86_400_000), None]
    );
    let delay = |ms: u64| cache_warming_delay(Duration::from_millis(ms)).map(|delay| delay.as_millis());
    assert_eq!([delay(300_000), delay(60_000), delay(10_000)], [Some(270_000), Some(50_000), None]);
    let reasoning = StreamOptions { reasoning: Some(ThinkingLevel::Medium), ..StreamOptions::default() };
    assert_eq!(
        [
            is_replayable(&budget(), &reasoning),
            is_replayable(&budget(), &StreamOptions::default()),
            is_replayable(&adaptive(), &reasoning),
            is_replayable(&openai, &reasoning),
        ],
        [false, true, true, true]
    );
}

#[tokio::test(start_paused = true)]
async fn replays_profitable_requests_and_keeps_options() {
    let fake = host(CacheWarmingMode::Idle, 100_000, None, ok_reply());
    let warmer = CacheWarmer::new(fake.clone());
    let original = CancellationToken::new();
    let options = StreamOptions {
        reasoning: Some(ThinkingLevel::High),
        session_id: Some("s".into()),
        cancel: original.clone(),
        ..StreamOptions::default()
    };
    warmer.start(request(adaptive(), options), current());
    assert_eq!(warmer.status().state, CacheWarmingState::Scheduled);
    advance(269_000).await;
    assert!(fake.calls.lock().unwrap().is_empty());
    advance(1_000).await;
    {
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!((call.reasoning, call.session_id.as_deref()), (Some(ThinkingLevel::High), Some("s")));
        assert_eq!((call.max_tokens, call.max_retries), (Some(1), Some(0)));
        original.cancel();
        assert!(!call.cancel.is_cancelled(), "the refresh has its own cancellation");
    }
    let event = fake.events.lock().unwrap()[0].clone();
    assert_eq!((event.continuation_probability, event.action), (1.0, CacheWarmingAction::Warm));
    assert!((event.miss_cost - 0.575).abs() < 1e-9);
    assert!((event.warm_cost - 0.050025).abs() < 1e-9);
    assert_eq!(event.to_json()["type"], "cache_warming_decision");
    assert_eq!(*fake.warmed.lock().unwrap(), [(100, None)]);
    advance(270_000).await;
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    warmer.cancel();
    assert_eq!(warmer.status().reason.as_deref(), Some("inactive"));
}

#[tokio::test(start_paused = true)]
async fn skips_refreshes_after_their_safe_deadline() {
    let fake = host(CacheWarmingMode::Idle, 100_000, None, ok_reply());
    let warmer = CacheWarmer::new(fake.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    let (id, timer) = {
        let mut inner = warmer.lock();
        let run = inner.run.as_mut().unwrap();
        (run.id, run.timer.take().unwrap())
    };
    timer.abort();
    // Scheduled for 4m30s with 15 s of the 30 s margin kept: a timer this late is skipped.
    tokio::time::advance(Duration::from_millis(285_001)).await;
    warmer.refresh(id).await;
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(warmer.status().reason.as_deref(), Some("cache refresh deadline missed"));
}

#[tokio::test(start_paused = true)]
async fn applies_economics_and_extension_overrides() {
    let unprofitable = host(CacheWarmingMode::Idle, 5_000, None, ok_reply());
    let warmer = CacheWarmer::new(unprofitable.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    advance(270_000).await;
    assert!(unprofitable.calls.lock().unwrap().is_empty());
    let status = warmer.status();
    assert_eq!(status.state, CacheWarmingState::Inactive);
    let decision = status.decision.unwrap();
    assert!(decision.economics_available && decision.action == CacheWarmingAction::Stop && !status.extension_override);

    let forced = host(CacheWarmingMode::Idle, 5_000, Some(CacheWarmingAction::Warm), ok_reply());
    let warmer = CacheWarmer::new(forced.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    advance(270_000).await;
    assert_eq!(*forced.warmed.lock().unwrap(), [(100, Some("extension override".into()))]);
    warmer.cancel();

    let vetoed = host(CacheWarmingMode::Idle, 100_000, Some(CacheWarmingAction::Stop), ok_reply());
    let warmer = CacheWarmer::new(vetoed.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    advance(270_000).await;
    assert!(vetoed.calls.lock().unwrap().is_empty());
    let status = warmer.status();
    assert!(status.extension_override);
    assert_eq!(status.reason.as_deref(), Some("stopped by extension"));

    let unavailable = host(CacheWarmingMode::Idle, 0, None, ok_reply());
    let warmer = CacheWarmer::new(unavailable.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    assert_eq!(warmer.status().reason.as_deref(), Some("cache economics unavailable"));
    advance(270_000).await;
    assert!(unavailable.calls.lock().unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn stops_for_unsupported_requests_context_and_mode_changes() {
    let fake = host(CacheWarmingMode::Off, 100_000, None, ok_reply());
    let warmer = CacheWarmer::new(fake.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    assert_eq!(warmer.status().reason.as_deref(), Some("cache warming disabled"));
    *fake.mode.lock().unwrap() = CacheWarmingMode::Idle;
    warmer.start(request(Model { prompt_cache: None, ..adaptive() }, StreamOptions::default()), current());
    assert_eq!(warmer.status().reason.as_deref(), Some("cache lifetime unavailable"));
    let none = StreamOptions { cache_retention: Some(CacheRetention::None), ..StreamOptions::default() };
    warmer.start(request(adaptive(), none), current());
    assert_eq!(warmer.status().reason.as_deref(), Some("request disabled prompt caching"));
    let reasoning = StreamOptions { reasoning: Some(ThinkingLevel::High), ..StreamOptions::default() };
    warmer.start(request(budget(), reasoning), current());
    assert_eq!(warmer.status().reason.as_deref(), Some("request cannot be replayed safely"));

    let still_current = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = still_current.clone();
    warmer.start(request(adaptive(), StreamOptions::default()), Arc::new(move || flag.load(Ordering::SeqCst)));
    still_current.store(false, Ordering::SeqCst);
    assert_eq!(warmer.status().reason.as_deref(), Some("conversation context changed"));
    advance(270_000).await;
    assert!(fake.calls.lock().unwrap().is_empty());

    warmer.start(request(adaptive(), StreamOptions::default()), current());
    *fake.mode.lock().unwrap() = CacheWarmingMode::Off;
    advance(270_000).await;
    assert!(fake.calls.lock().unwrap().is_empty());

    let streaming = host(CacheWarmingMode::Streaming, 400_000, None, ok_reply());
    let warmer = CacheWarmer::new(streaming.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    warmer.on_agent_settled();
    assert_eq!(warmer.status().reason.as_deref(), Some("agent run settled"));

    let idle = host(CacheWarmingMode::Idle, 400_000, None, ok_reply());
    let warmer = CacheWarmer::new(idle.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    warmer.on_agent_settled();
    let decision = warmer.status().decision.unwrap();
    assert_eq!((decision.phase, decision.continuation_probability), (CacheWarmingPhase::Idle, 0.15));
    *idle.mode.lock().unwrap() = CacheWarmingMode::Streaming;
    warmer.on_mode_changed();
    assert_eq!(warmer.status().reason.as_deref(), Some("agent run settled"));
}

#[tokio::test(start_paused = true)]
async fn idle_warming_ends_at_the_safety_limit() {
    let fake = host(CacheWarmingMode::Idle, 400_000, None, ok_reply());
    let warmer = CacheWarmer::new(fake.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    warmer.on_agent_settled();
    advance(30 * 60_000).await;
    // 30 minutes allow six refreshes 4m30s apart; the seventh would pass the limit.
    assert_eq!(fake.calls.lock().unwrap().len(), 6);
    assert_eq!(warmer.status().reason.as_deref(), Some("30-minute idle safety limit reached"));
}

#[tokio::test(start_paused = true)]
async fn aborts_replaced_requests_and_skips_failed_refreshes() {
    let (release, released) = oneshot::channel::<()>();
    let released = Mutex::new(Some(released));
    let pending = host(
        CacheWarmingMode::Idle,
        100_000,
        None,
        Box::new(move |model| {
            let (sender, stream) = event_channel();
            let wait = released.lock().unwrap().take();
            let model = model.clone();
            tokio::spawn(async move {
                if let Some(wait) = wait {
                    let _ = wait.await;
                }
                let mut message = AssistantMessage::empty(&model);
                message.usage = warm_usage();
                sender.push(pi_ai::AssistantMessageEvent::Done { reason: StopReason::Length, message });
            });
            stream
        }),
    );
    let warmer = CacheWarmer::new(pending.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    advance(270_000).await;
    assert_eq!(warmer.status().state, CacheWarmingState::Refreshing);
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    assert!(pending.calls.lock().unwrap()[0].cancel.is_cancelled());
    let _ = release.send(());
    warmer.cancel();
    advance(600_000).await;
    assert_eq!(pending.calls.lock().unwrap().len(), 1);
    assert!(pending.warmed.lock().unwrap().is_empty());

    let failed =
        host(CacheWarmingMode::Idle, 100_000, None, Box::new(|model| error_stream(model, StopReason::Error, "x")));
    let warmer = CacheWarmer::new(failed.clone());
    warmer.start(request(adaptive(), StreamOptions::default()), current());
    advance(270_000).await;
    assert_eq!(failed.calls.lock().unwrap().len(), 1);
    assert!(failed.warmed.lock().unwrap().is_empty());
    warmer.cancel();
}

#[test]
fn formats_status() {
    let decision = CacheWarmingDecision {
        phase: CacheWarmingPhase::Idle,
        warm_cost: 0.013,
        miss_cost: 0.621,
        continuation_probability: 0.6,
        expected_savings: 0.36,
        economics_available: true,
        action: CacheWarmingAction::Warm,
    };
    let scheduled = CacheWarmingStatus {
        state: CacheWarmingState::Scheduled,
        reason: None,
        next_warm_at: Some(222_000),
        decision: Some(decision.clone()),
        extension_override: false,
    };
    assert_eq!(
        format_cache_warming_status(&scheduled, 0),
        "Decision in 3m 42s (60% continuation probability, expected savings $0.360 >= $0.050 -> warm)"
    );
    assert_eq!(
        format_cache_warming_status(&CacheWarmingStatus::inactive("agent run settled"), 0),
        "Inactive (agent run settled)"
    );
    let stopped = CacheWarmingStatus {
        state: CacheWarmingState::Inactive,
        decision: Some(CacheWarmingDecision {
            phase: CacheWarmingPhase::Streaming,
            expected_savings: -0.01,
            action: CacheWarmingAction::Stop,
            continuation_probability: 1.0,
            ..decision
        }),
        ..CacheWarmingStatus::inactive("expected savings below threshold")
    };
    assert_eq!(
        format_cache_warming_status(&stopped, 0),
        "Stopped (100% continuation probability while agent is running, expected savings -$0.010 < $0.050 -> stop)"
    );
}
