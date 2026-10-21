//! One read-only pass over a session file: prompts, assistant turns, and billed usage, each
//! with its time.

use std::path::Path;

use pi_ai::Message;
use serde_json::Value;

use crate::session::entry::{Entry, EntryData};
use crate::session::parse_iso;
use crate::session::storage::parse_session_entries;
use crate::session::usage::{UsageTotals, entry_usage};

/// Billed usage of one entry.
#[derive(Clone, Debug, PartialEq)]
pub struct EventUsage {
    /// `provider/model`, or `Tools/summaries` for tool results and summaries, as `/session` shows.
    pub model: String,
    /// The provider, or the `model` key when the entry names none.
    pub provider: String,
    pub totals: UsageTotals,
}

/// A user message, an assistant message, or another entry with billed usage.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionEvent {
    /// Milliseconds since the epoch; `None` when the entry has no readable time.
    pub time: Option<i64>,
    pub prompt: bool,
    pub assistant: bool,
    pub usage: Option<EventUsage>,
}

/// The events of a session file, in file order. The file is only read: no migration, no repair.
pub fn scan_session_file(path: &Path) -> Vec<SessionEvent> {
    let Ok(bytes) = std::fs::read(path) else { return Vec::new() };
    let entries = parse_session_entries(&String::from_utf8_lossy(&bytes));
    entries.into_iter().skip(1).filter_map(|raw| event(&Entry::from_raw(raw))).collect()
}

/// Total cost of a session file, counted as `/session` counts it.
pub fn session_cost(path: &Path) -> f64 {
    scan_session_file(path).iter().filter_map(|event| event.usage.as_ref()).map(|usage| usage.totals.cost).sum()
}

fn event(entry: &Entry) -> Option<SessionEvent> {
    let message = entry.message();
    let prompt = matches!(message, Some(Message::User(_)));
    let assistant = matches!(message, Some(Message::Assistant(_)));
    let usage = entry_usage(entry).map(|(model, usage)| {
        let provider = match entry.data() {
            EntryData::Message(Message::Assistant(assistant)) => assistant.provider.clone(),
            EntryData::Usage(usage) => usage.provider.clone(),
            _ => model.clone(),
        };
        let mut totals = UsageTotals::default();
        totals.add(usage);
        EventUsage { model, provider, totals }
    });
    if !prompt && !assistant && usage.is_none() {
        return None;
    }
    Some(SessionEvent { time: entry_time(entry), prompt, assistant, usage })
}

/// The message time of a message entry, else the entry timestamp.
fn entry_time(entry: &Entry) -> Option<i64> {
    let message = entry.raw().get("message").and_then(|message| message.get("timestamp"));
    let message_time = message.and_then(Value::as_f64).map(|time| time as i64).filter(|time| *time > 0);
    message_time.or_else(|| parse_iso(entry.timestamp()))
}
