//! Prompt cache misses (upstream `core/cache-stats.ts`): prompt tokens of the last request that
//! were billed again instead of read from the cache, and the transcript notice for them.

use std::collections::HashMap;

use pi_ai::{AssistantMessage, Message, Usage};

use crate::session::entry::{Entry, EntryData, UsageEntry};

/// Idle gaps longer than this probably expired the cache (Anthropic's default TTL).
pub const CACHE_TTL_MS: i64 = 5 * 60 * 1000;

/// Misses at or below this many tokens are cache breakpoint noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// Notices are shown from this many missed tokens or this many dollars.
const NOTICE_MIN_TOKENS: u64 = 20_000;
const NOTICE_MIN_COST: f64 = 0.1;

/// A counted miss on one assistant message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CacheMiss {
    /// Prompt tokens of the last request that were not read from the cache.
    pub missed_tokens: u64,
    /// Extra dollars paid compared to a full hit; 0 without pricing.
    pub missed_cost: f64,
    /// Milliseconds since the last request, which last refreshed the cache.
    pub idle_ms: i64,
    /// The model changed since the last request.
    pub model_changed: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    /// Counted misses (turns above the noise floor).
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

/// The last request of the scan; everything in its prompt should be cached now.
struct LastRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp: i64,
    /// Some request of this scan segment reported cache activity, which tells a total miss on a
    /// cache-read-only provider apart from a provider that never reports caching.
    reported_cache: bool,
}

fn prompt_tokens(usage: &Usage) -> u64 {
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
    // Missed tokens land in the input or cache write buckets, so this message's own costs give
    // the rate actually paid for them.
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
            // The context changed on purpose; model switches are not exempt.
            EntryData::Compaction(_) | EntryData::BranchSummary(_) => result.last = None,
            EntryData::Usage(usage) if usage.kind == "cache_warm" => {
                let tokens = prompt_tokens(&usage.usage);
                if tokens > 0 {
                    result.last = Some(LastRequest {
                        prompt_tokens: tokens,
                        model_key: format!("{}/{}", usage.provider, usage.model),
                        timestamp: crate::session::parse_iso(entry.timestamp()).unwrap_or(0),
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

/// Token counts as the footer shows them: `950`, `1.2k`, `45k`, `1.5M`, `12M`.
pub fn format_tokens(count: u64) -> String {
    let value = count as f64;
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{:.1}k", value / 1_000.0),
        10_000..1_000_000 => format!("{}k", (value / 1_000.0).round()),
        1_000_000..10_000_000 => format!("{:.1}M", value / 1_000_000.0),
        _ => format!("{}M", (value / 1_000_000.0).round()),
    }
}

/// The transcript notice for `miss`, or `None` below 20k tokens and $0.10. It only states what
/// is known: the miss, a model switch, or an idle gap past the cache TTL.
pub fn cache_miss_notice(miss: &CacheMiss) -> Option<String> {
    if miss.missed_tokens < NOTICE_MIN_TOKENS && miss.missed_cost < NOTICE_MIN_COST {
        return None;
    }
    let cost = if miss.missed_cost >= 0.01 { format!(" (~${:.2})", miss.missed_cost) } else { String::new() };
    let label = if miss.model_changed {
        "Cache miss after model switch".to_string()
    } else if miss.idle_ms >= CACHE_TTL_MS {
        format!("Cache miss after {}m idle", (miss.idle_ms as f64 / 60_000.0).round())
    } else {
        "Cache miss".to_string()
    };
    Some(format!("{label}: {} tokens re-billed{cost}", format_tokens(miss.missed_tokens)))
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
#[path = "cache_stats_tests.rs"]
mod tests;
