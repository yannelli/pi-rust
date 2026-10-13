//! Prompt cache misses: prompt tokens of the last request that were billed again instead of
//! read from the cache (upstream `core/cache-stats.ts` and `formatCacheWarmingUsage`).

use std::collections::HashMap;

use pi_ai::{AssistantMessage, Message};

use crate::session::entry::{Entry, EntryData, UsageEntry};

/// Idle gaps longer than this probably expired the cache (Anthropic's default TTL).
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Misses at or below this many tokens are cache breakpoint noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A counted miss on one assistant message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CacheMiss {
    pub missed_tokens: u64,
    /// Extra dollars paid compared to a full hit; 0 without pricing.
    pub missed_cost: f64,
    /// Milliseconds since the last request.
    pub idle_ms: i64,
    pub model_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    pub miss_count: u64,
}

/// Cache-read price lookup in dollars per million tokens.
pub trait ModelPriceSource {
    fn cache_read_price(&self, provider: &str, model: &str) -> Option<f64>;
}

impl ModelPriceSource for crate::agent_session::ModelRuntime {
    fn cache_read_price(&self, provider: &str, model: &str) -> Option<f64> {
        self.get_model(provider, model).map(|model| model.cost.cache_read)
    }
}

struct LastRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: i64,
    /// Some request so far reported cache activity.
    reported_cache: bool,
}

fn prompt_tokens(usage: &pi_ai::Usage) -> u64 {
    usage.input + usage.cache_read + usage.cache_write
}

fn detect_miss(
    last: Option<&LastRequest>,
    message: &AssistantMessage,
    prices: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let usage = &message.usage;
    let tokens = prompt_tokens(usage);
    let last = last?;
    if tokens == 0 || (usage.cache_read + usage.cache_write == 0 && !last.reported_cache) {
        return None;
    }
    let missed_tokens = last.prompt_tokens.min(tokens).saturating_sub(usage.cache_read);
    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }
    let paid_tokens = usage.input + usage.cache_write;
    let paid_per_token =
        if paid_tokens > 0 { (usage.cost.input + usage.cost.cache_write) / paid_tokens as f64 } else { 0.0 };
    let read_per_token = if usage.cache_read > 0 {
        usage.cost.cache_read / usage.cache_read as f64
    } else {
        prices.cache_read_price(&message.provider, &message.model).unwrap_or(0.0) / 1_000_000.0
    };
    Some(CacheMiss {
        missed_tokens,
        missed_cost: missed_tokens as f64 * (paid_per_token - read_per_token).max(0.0),
        idle_ms: (message.timestamp - last.timestamp).max(0),
        model_changed: format!("{}/{}", message.provider, message.model) != last.model_key,
    })
}

fn as_last_request(message: &AssistantMessage, reported_cache: bool) -> Option<LastRequest> {
    let usage = &message.usage;
    let tokens = prompt_tokens(usage);
    (tokens > 0).then(|| LastRequest {
        prompt_tokens: tokens,
        model_key: format!("{}/{}", message.provider, message.model),
        timestamp: message.timestamp,
        reported_cache: reported_cache || usage.cache_read + usage.cache_write > 0,
    })
}

/// Milliseconds of an RFC 3339 timestamp, 0 when it does not parse.
fn timestamp_millis(timestamp: &str) -> i64 {
    timestamp.parse::<jiff::Timestamp>().map(|parsed| parsed.as_millisecond()).unwrap_or(0)
}

struct Scan {
    last: Option<LastRequest>,
    totals: CacheWasteTotals,
    /// Misses keyed by the id of the entry holding the assistant message.
    misses: HashMap<String, CacheMiss>,
}

fn scan<'a>(entries: impl IntoIterator<Item = &'a Entry>, prices: &dyn ModelPriceSource) -> Scan {
    let mut result = Scan { last: None, totals: CacheWasteTotals::default(), misses: HashMap::new() };
    for entry in entries {
        match entry.data() {
            EntryData::Compaction(_) | EntryData::BranchSummary(_) => result.last = None,
            EntryData::Usage(usage) if usage.kind == "cache_warm" => {
                let tokens = prompt_tokens(&usage.usage);
                if tokens > 0 {
                    result.last = Some(LastRequest {
                        prompt_tokens: tokens,
                        model_key: format!("{}/{}", usage.provider, usage.model),
                        timestamp: timestamp_millis(entry.timestamp()),
                        reported_cache: true,
                    });
                }
            }
            EntryData::Message(Message::Assistant(message)) => {
                if let Some(miss) = detect_miss(result.last.as_ref(), message, prices) {
                    result.totals.missed_tokens += miss.missed_tokens;
                    result.totals.missed_cost += miss.missed_cost;
                    result.totals.miss_count += 1;
                    result.misses.insert(entry.id().to_string(), miss);
                }
                let reported = result.last.as_ref().is_some_and(|last| last.reported_cache);
                if let Some(last) = as_last_request(message, reported) {
                    result.last = Some(last);
                }
            }
            _ => {}
        }
    }
    result
}

/// Cache waste across `entries`.
pub fn compute_cache_waste<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    prices: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    scan(entries, prices).totals
}

/// Counted misses by assistant message entry id, to show notices again when rebuilding the chat.
pub fn collect_cache_misses<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    prices: &dyn ModelPriceSource,
) -> HashMap<String, CacheMiss> {
    scan(entries, prices).misses
}

/// The miss of a just-completed `message`; `entries` do not contain it yet.
pub fn detect_cache_miss<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
    message: &AssistantMessage,
    prices: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    detect_miss(scan(entries, prices).last.as_ref(), message, prices)
}

/// `Cache warmed (<note>): $<cost>` with at least three decimals.
pub fn format_cache_warming_usage(entry: &UsageEntry) -> String {
    let note = entry.note.as_deref().map(|note| format!(" ({note})")).unwrap_or_default();
    let mut cost = format!("{:.6}", entry.usage.cost.total);
    while cost.ends_with('0') && cost.split_once('.').is_some_and(|(_, decimals)| decimals.len() > 3) {
        cost.pop();
    }
    format!("Cache warmed{note}: ${cost}")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoPrices;

    impl ModelPriceSource for NoPrices {
        fn cache_read_price(&self, _: &str, _: &str) -> Option<f64> {
            None
        }
    }

    fn assistant(input: u64, cache_read: u64, cache_write: u64, timestamp: i64) -> AssistantMessage {
        let mut message =
            AssistantMessage { provider: "anthropic".into(), model: "claude".into(), timestamp, ..Default::default() };
        message.usage.input = input;
        message.usage.cache_read = cache_read;
        message.usage.cache_write = cache_write;
        message.usage.cost.input = input as f64 * 3e-6;
        message.usage.cost.cache_read = cache_read as f64 * 0.3e-6;
        message
    }

    #[test]
    fn counts_misses_above_the_noise_floor() {
        let last = as_last_request(&assistant(100, 50_000, 1000, 0), false);
        let miss = detect_miss(last.as_ref(), &assistant(40_000, 11_000, 0, 400_000), &NoPrices).unwrap();
        assert_eq!(miss.missed_tokens, 40_000);
        assert!((miss.missed_cost - 40_000.0 * 2.7e-6).abs() < 1e-9);
        assert_eq!(miss.idle_ms, 400_000);
        assert!(!miss.model_changed);
        assert!(detect_miss(last.as_ref(), &assistant(500, 50_600, 0, 1), &NoPrices).is_none());
    }

    #[test]
    fn ignores_providers_without_cache_reports() {
        let last = as_last_request(&assistant(50_000, 0, 0, 0), false);
        assert!(detect_miss(last.as_ref(), &assistant(60_000, 0, 0, 1), &NoPrices).is_none());
    }

    #[test]
    fn formats_cache_warming_cost() {
        let mut entry = UsageEntry {
            kind: "cache_warm".into(),
            provider: "anthropic".into(),
            model: "claude".into(),
            usage: pi_ai::Usage::default(),
            note: Some("idle".into()),
        };
        entry.usage.cost.total = 0.0123;
        assert_eq!(format_cache_warming_usage(&entry), "Cache warmed (idle): $0.0123");
        entry.usage.cost.total = 0.5;
        assert_eq!(format_cache_warming_usage(&entry), "Cache warmed (idle): $0.500");
    }
}
