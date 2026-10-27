//! Token and cost totals over a whole session, including tool-result usage, `usage`
//! entries, and summary generation. Port of upstream `core/usage-totals.ts` and
//! `AgentSession.getSessionStats`.

use std::collections::HashMap;
use std::path::PathBuf;

use pi_ai::{Message, Usage};

use super::SessionManager;
use super::entry::{Entry, EntryData};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl UsageTotals {
    pub fn add(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cache_write += usage.cache_write;
        self.cost += usage.cost.total;
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// Sum of two usages (upstream `combineUsage`); shared with nested tool calls.
pub use pi_agent::combine_usage;

/// Usage an entry adds to session totals, with its breakdown key: assistant messages and
/// `usage` entries by `provider/model`, tool results and summaries under `Tools/summaries`.
pub fn entry_usage(entry: &Entry) -> Option<(String, &Usage)> {
    match entry.data() {
        EntryData::Message(Message::Assistant(assistant)) => {
            let model = assistant.response_model.as_deref().unwrap_or(&assistant.model);
            Some((format!("{}/{model}", assistant.provider), &assistant.usage))
        }
        EntryData::Usage(usage) => Some((format!("{}/{}", usage.provider, usage.model), &usage.usage)),
        EntryData::Message(Message::ToolResult(result)) => {
            result.usage.as_ref().map(|usage| ("Tools/summaries".to_string(), usage))
        }
        EntryData::Compaction(compaction) => compaction.usage.as_ref().map(|usage| ("Tools/summaries".into(), usage)),
        EntryData::BranchSummary(summary) => summary.usage.as_ref().map(|usage| ("Tools/summaries".into(), usage)),
        _ => None,
    }
}

/// Totals over `entries`, counting every source of billed usage.
pub fn usage_totals<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> UsageTotals {
    let mut totals = UsageTotals::default();
    for entry in entries {
        if let Some((_, usage)) = entry_usage(entry) {
            totals.add(usage);
        }
    }
    totals
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageCostBreakdownEntry {
    pub key: String,
    pub cost: f64,
    pub tokens: u64,
}

/// Cost and tokens per model, plus a `Tools/summaries` bucket; most expensive first.
pub fn usage_cost_breakdown<'a>(entries: impl IntoIterator<Item = &'a Entry>) -> Vec<UsageCostBreakdownEntry> {
    let mut order: Vec<String> = Vec::new();
    let mut totals: HashMap<String, UsageTotals> = HashMap::new();
    for entry in entries {
        let Some((key, usage)) = entry_usage(entry) else { continue };
        if !totals.contains_key(&key) {
            order.push(key.clone());
        }
        totals.entry(key).or_default().add(usage);
    }
    let mut breakdown: Vec<UsageCostBreakdownEntry> = order
        .into_iter()
        .map(|key| {
            let totals = totals[&key];
            UsageCostBreakdownEntry { key, cost: totals.cost, tokens: totals.tokens() }
        })
        .filter(|entry| entry.cost > 0.0 || entry.tokens > 0)
        .collect();
    breakdown.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    breakdown
}

/// Counts and totals for `/session`, over all entries including compacted history.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub session_file: Option<PathBuf>,
    pub session_id: String,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub total_messages: usize,
    pub tokens: UsageTotals,
    pub cost: f64,
}

impl SessionManager {
    pub fn session_stats(&self) -> SessionStats {
        let mut stats = SessionStats {
            session_file: self.session_file().map(PathBuf::from),
            session_id: self.session_id().to_string(),
            ..SessionStats::default()
        };
        for entry in self.entries() {
            if let Some(message) = entry.message() {
                stats.total_messages += 1;
                match message {
                    Message::User(_) => stats.user_messages += 1,
                    Message::ToolResult(_) => stats.tool_results += 1,
                    Message::Assistant(assistant) => {
                        stats.assistant_messages += 1;
                        stats.tool_calls += assistant.tool_calls().count();
                    }
                    _ => {}
                }
            }
        }
        stats.tokens = usage_totals(self.entries());
        stats.cost = stats.tokens.cost;
        stats
    }
}
