//! Cache warming and cache-miss notices of a session over a scripted stream function that
//! reports cache usage (upstream `sdk.ts` warmer wiring and the interactive cache-miss notice).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use pi_agent::StreamFn;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, ContentBlock, Model, StopReason, StreamOptions, Usage, event_channel,
};
use pi_coding_agent::cache_warmer::{CacheWarmingAction, CacheWarmingPhase, CacheWarmingState};
use pi_coding_agent::settings::schema::CacheWarmingMode;
use serde_json::{Value, json};

use crate::harness::{Harness, Options, harness_with};

#[derive(Default)]
struct Script {
    usages: Mutex<VecDeque<Usage>>,
    calls: Mutex<Vec<StreamOptions>>,
}

fn scripted(script: Arc<Script>) -> StreamFn {
    Arc::new(move |model: &Model, _context: &pi_ai::Context, options: StreamOptions| {
        script.calls.lock().unwrap().push(options.clone());
        let usage = script.usages.lock().unwrap().pop_front().unwrap_or_default();
        let (sender, stream) = event_channel();
        let mut message = AssistantMessage::empty(model);
        message.content = vec![ContentBlock::text("ok")];
        message.usage = usage;
        message.stop_reason = if options.max_tokens == Some(1) { StopReason::Length } else { StopReason::Stop };
        message.timestamp = pi_ai::now_ms();
        sender.push(AssistantMessageEvent::Done { reason: message.stop_reason, message });
        stream
    })
}

fn usage(cache_read: u64, cache_write: u64, cost_read: f64, cost_write: f64) -> Usage {
    let mut usage = Usage { output: 10, cache_read, cache_write, ..Usage::default() };
    usage.cost.cache_read = cost_read;
    usage.cost.cache_write = cost_write;
    usage.cost.total = cost_read + cost_write;
    usage
}

async fn session(settings: Value, usages: Vec<Usage>) -> (Harness, Arc<Script>) {
    let script = Arc::new(Script { usages: Mutex::new(usages.into()), calls: Mutex::default() });
    let stream = scripted(script.clone());
    let options = Options {
        settings,
        // A 12-second cache lifetime: refreshes are due after two seconds.
        tiny: json!({"promptCache": {"short": 12}, "cost": {"input": 3, "output": 15, "cacheRead": 0.3, "cacheWrite": 30}}),
        ..Options::default()
    };
    let h = harness_with(options, |create| create.stream_fn = Some(stream)).await;
    (h, script)
}

#[tokio::test(start_paused = true)]
async fn warms_an_idle_session_and_records_usage_entries() {
    let (h, script) =
        session(json!({"cacheWarming": "idle"}), vec![usage(0, 20_000, 0.0, 0.6), usage(20_000, 0, 0.006, 0.0)]).await;
    h.session.prompt("hello", Vec::new()).await.unwrap();
    let status = h.session.cache_warming_status();
    assert_eq!(status.state, CacheWarmingState::Scheduled);
    let decision = status.decision.unwrap();
    assert_eq!((decision.phase, decision.action), (CacheWarmingPhase::Idle, CacheWarmingAction::Warm));

    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let calls = script.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "the request was sent again");
    assert_eq!((calls[1].max_tokens, calls[1].max_retries), (Some(1), Some(0)));
    let warm: Vec<Value> = h.entries().into_iter().filter(|entry| entry["type"] == "usage").collect();
    assert_eq!(warm.len(), 1);
    assert_eq!((warm[0]["kind"].as_str(), warm[0]["provider"].as_str()), (Some("cache_warm"), Some("local")));
    assert_eq!((warm[0]["model"].as_str(), warm[0]["usage"]["cacheRead"].as_u64()), (Some("tiny"), Some(20_000)));
    assert!(h.events_of("entry_appended").iter().any(|event| event["entry"]["type"] == "usage"));
    assert_eq!(h.session.messages().len(), 3, "warming leaves the transcript alone");

    h.session.set_cache_warming_mode(CacheWarmingMode::Off);
    assert_eq!(h.session.cache_warming_status().reason.as_deref(), Some("cache warming disabled"));
    assert_eq!(h.settings.cache_warming_mode(), CacheWarmingMode::Off);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(script.calls.lock().unwrap().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn streaming_mode_stops_when_the_run_settles() {
    let (h, script) = session(json!({}), vec![usage(0, 20_000, 0.0, 0.6)]).await;
    h.session.prompt("hello", Vec::new()).await.unwrap();
    assert_eq!(h.session.cache_warming_status().reason.as_deref(), Some("agent run settled"));
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(script.calls.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn extensions_decide_through_the_cache_warming_hook() {
    let (h, script) = session(json!({"cacheWarming": "idle"}), vec![usage(0, 20_000, 0.0, 0.6)]).await;
    let events: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = events.clone();
    h.session.set_cache_warming_decider(Some(Arc::new(move |event| {
        sink.lock().unwrap().push(event.to_json());
        async { Some(CacheWarmingAction::Stop) }.boxed()
    })));
    h.session.prompt("hello", Vec::new()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    assert_eq!(script.calls.lock().unwrap().len(), 1);
    let events = events.lock().unwrap().clone();
    assert_eq!(
        (events[0]["type"].as_str(), events[0]["action"].as_str()),
        (Some("cache_warming_decision"), Some("warm"))
    );
    let status = h.session.cache_warming_status();
    assert_eq!((status.reason.as_deref(), status.extension_override), (Some("stopped by extension"), true));
}

#[tokio::test]
async fn emits_cache_miss_notices_when_enabled() {
    let usages =
        || vec![usage(0, 100_000, 0.0, 0.375), usage(100_000, 5_000, 0.03, 0.019), usage(0, 110_000, 0.0, 0.4125)];
    let (h, _) = session(json!({"showCacheMissNotices": true, "compaction": {"enabled": false}}), usages()).await;
    for prompt in ["one", "two", "three"] {
        h.session.prompt(prompt, Vec::new()).await.unwrap();
    }
    let misses = h.events_of("cache_miss");
    assert_eq!(misses.len(), 1);
    assert_eq!(misses[0]["missedTokens"], 105_000);
    assert_eq!(misses[0]["modelChanged"], false);
    assert_eq!(misses[0]["notice"], "Cache miss: 105k tokens re-billed (~$0.36)");
    let kinds: Vec<String> = h.events().iter().map(|event| event["type"].as_str().unwrap().to_string()).collect();
    let at = kinds.iter().position(|kind| kind == "cache_miss").unwrap();
    assert_eq!(kinds[at - 1], "message_end", "the notice follows the response");

    let (quiet, _) = session(json!({"compaction": {"enabled": false}}), usages()).await;
    for prompt in ["one", "two", "three"] {
        quiet.session.prompt(prompt, Vec::new()).await.unwrap();
    }
    assert!(quiet.events_of("cache_miss").is_empty());
}
