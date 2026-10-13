//! Port of upstream `test/cache-stats.test.ts`.

use pi_ai::{AssistantMessage, StopReason};
use serde_json::{Value, json};

use super::*;

struct Prices;

impl ModelPriceSource for Prices {
    fn cache_read_price(&self, _: &str, _: &str) -> Option<f64> {
        Some(0.3)
    }
}

#[derive(Default)]
struct Turn {
    input: u64,
    cache_read: u64,
    cache_write: u64,
    cost_read: f64,
    cost_write: f64,
    model: Option<&'static str>,
    timestamp: i64,
}

fn assistant(turn: Turn) -> AssistantMessage {
    let mut message = AssistantMessage {
        api: "anthropic-messages".into(),
        provider: "test".into(),
        model: turn.model.unwrap_or("test-model").into(),
        stop_reason: StopReason::Stop,
        timestamp: turn.timestamp,
        ..Default::default()
    };
    message.usage.input = turn.input;
    message.usage.output = 10;
    message.usage.cache_read = turn.cache_read;
    message.usage.cache_write = turn.cache_write;
    message.usage.cost.cache_read = turn.cost_read;
    message.usage.cost.cache_write = turn.cost_write;
    message
}

fn raw(value: Value) -> Entry {
    Entry::from_raw(value.as_object().unwrap().clone())
}

fn entry(id: &str, message: &AssistantMessage) -> Entry {
    raw(
        json!({"type": "message", "id": id, "parentId": null, "timestamp": "", "message": Message::Assistant(message.clone())}),
    )
}

fn usage_entry(kind: &str, timestamp: i64) -> Entry {
    let timestamp = crate::session::ids::format_iso(timestamp);
    raw(json!({"type": "usage", "id": format!("usage-{kind}"), "parentId": null, "timestamp": timestamp,
        "kind": kind, "provider": "test", "model": "test-model",
        "usage": {"input": 0, "output": 0, "cacheRead": 100_000, "cacheWrite": 0, "totalTokens": 100_000,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}}))
}

fn turn1() -> AssistantMessage {
    assistant(Turn { cache_write: 100_000, cost_write: 0.375, ..Turn::default() })
}

fn turn2() -> AssistantMessage {
    assistant(Turn {
        cache_read: 100_000,
        cache_write: 5_000,
        cost_read: 0.03,
        cost_write: 0.019,
        timestamp: 60_000,
        ..Turn::default()
    })
}

fn miss_turn(timestamp: i64, model: Option<&'static str>) -> AssistantMessage {
    assistant(Turn { cache_write: 110_000, cost_write: 0.4125, timestamp, model, ..Turn::default() })
}

#[test]
fn accumulates_missed_tokens_and_cost() {
    let entries = [entry("1", &turn1()), entry("2", &turn2()), entry("3", &miss_turn(120_000, None))];
    let totals = compute_cache_waste(&entries, &Prices);
    assert_eq!(totals.missed_tokens, 105_000);
    assert!((totals.missed_cost - 0.36225).abs() < 1e-5);
    assert_eq!(compute_cache_waste(&entries[..2], &Prices), CacheWasteTotals::default());
}

#[test]
fn compaction_resets_and_model_switches_count() {
    let reset = raw(json!({"type": "compaction", "id": "c", "parentId": null, "timestamp": "", "summary": "s",
        "firstKeptEntryId": "c", "tokensBefore": 1}));
    let after = assistant(Turn { cache_write: 20_000, cost_write: 0.075, ..Turn::default() });
    assert_eq!(compute_cache_waste(&[entry("1", &turn1()), reset, entry("2", &after)], &Prices).missed_tokens, 0);
    let other = assistant(Turn { cache_write: 100_000, cost_write: 0.375, model: Some("other"), ..Turn::default() });
    let totals = compute_cache_waste(&[entry("1", &turn1()), entry("2", &other)], &Prices);
    assert_eq!((totals.missed_tokens, totals.miss_count), (100_000, 1));
    let (a, b) =
        (assistant(Turn { input: 100_000, ..Turn::default() }), assistant(Turn { input: 110_000, ..Turn::default() }));
    assert_eq!(compute_cache_waste(&[entry("a", &a), entry("b", &b)], &Prices).missed_tokens, 0);
}

#[test]
fn collects_misses_by_entry_id() {
    let entries = [entry("1", &turn1()), entry("2", &turn2()), entry("3", &miss_turn(120_000, None))];
    let misses = collect_cache_misses(&entries, &Prices);
    assert_eq!(misses.len(), 1);
    assert_eq!(misses["3"].missed_tokens, 105_000);
}

#[test]
fn detects_misses_with_idle_time_and_model_switches() {
    let entries = [entry("1", &turn1()), entry("2", &turn2())];
    let miss = detect_cache_miss(&entries, &miss_turn(600_000, None), &Prices).unwrap();
    assert_eq!((miss.missed_tokens, miss.idle_ms, miss.model_changed), (105_000, 540_000, false));
    assert!((miss.missed_cost - 0.36225).abs() < 1e-5);
    let switched = detect_cache_miss(&entries, &miss_turn(120_000, Some("other")), &Prices).unwrap();
    assert!(switched.model_changed);
    let healthy = assistant(Turn {
        cache_read: 105_000,
        cache_write: 2_000,
        cost_read: 0.0315,
        cost_write: 0.0075,
        timestamp: 120_000,
        ..Turn::default()
    });
    assert_eq!(detect_cache_miss(&entries, &healthy, &Prices), None);
    assert_eq!(detect_cache_miss(&[], &turn1(), &Prices), None);
}

#[test]
fn only_cache_warm_usage_refreshes_the_cache() {
    let message = miss_turn(600_000, None);
    let warm = detect_cache_miss(&[entry("1", &turn1()), usage_entry("cache_warm", 500_000)], &message, &Prices);
    let other = detect_cache_miss(&[entry("1", &turn1()), usage_entry("custom_operation", 500_000)], &message, &Prices);
    assert_eq!(warm.unwrap().idle_ms, 100_000);
    assert_eq!(other.unwrap().idle_ms, 600_000);
}

#[test]
fn notices_name_the_cause_above_the_display_floor() {
    let miss = CacheMiss { missed_tokens: 105_000, missed_cost: 0.36225, idle_ms: 540_000, model_changed: false };
    assert_eq!(cache_miss_notice(&miss).unwrap(), "Cache miss after 9m idle: 105k tokens re-billed (~$0.36)");
    let switched = CacheMiss { model_changed: true, idle_ms: 1_000, missed_cost: 0.005, ..miss };
    assert_eq!(cache_miss_notice(&switched).unwrap(), "Cache miss after model switch: 105k tokens re-billed");
    let small = CacheMiss { missed_tokens: 5_000, missed_cost: 0.02, idle_ms: 0, model_changed: false };
    assert_eq!(cache_miss_notice(&small), None);
    let costly = CacheMiss { missed_cost: 0.2, ..small };
    assert_eq!(cache_miss_notice(&costly).unwrap(), "Cache miss: 5.0k tokens re-billed (~$0.20)");
    assert_eq!([999, 1_234, 45_600, 1_500_000, 12_400_000].map(format_tokens), ["999", "1.2k", "46k", "1.5M", "12M"]);
}

#[test]
fn formats_cache_warming_cost() {
    let mut entry = UsageEntry {
        kind: "cache_warm".into(),
        provider: "anthropic".into(),
        model: "claude".into(),
        usage: Usage::default(),
        note: Some("extension override".into()),
    };
    entry.usage.cost.total = 0.0123;
    assert_eq!(format_cache_warming_usage(&entry), "Cache warmed (extension override): $0.0123");
    entry.usage.cost.total = 0.5;
    entry.note = None;
    assert_eq!(format_cache_warming_usage(&entry), "Cache warmed: $0.500");
}
