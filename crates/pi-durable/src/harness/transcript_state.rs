//! What a `ConversationTranscript` holds: entries, gaps, and the known range, and how a range joins them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::future::{BoxFuture, Shared};
use pi_chord::Json;

use super::transcript::{TranscriptItem, TranscriptView};
use crate::errors::DurableResult;
use crate::ids::EntryId;
use crate::types::EntryRecord;

pub(super) type Load = Shared<BoxFuture<'static, DurableResult<Arc<TranscriptView>>>>;
/// An unbounded end of the known range (upstream `-Infinity`).
pub(super) const NONE: i64 = i64::MIN;

#[derive(Default)]
pub(super) struct State {
    pub entries: Vec<EntryRecord>,
    pub ids: HashSet<EntryId>,
    /// Gaps in ID order as `(after, up_to)`.
    pub gaps: Vec<(i64, i64)>,
    pub started: bool,
    /// Exclusive lower bound of the known range; `NONE` once the start is known.
    pub low: i64,
    /// Inclusive upper bound: the newest entry, or a later empty window's `before`.
    pub tail: i64,
    pub items: Arc<Vec<TranscriptItem>>,
    pub last: Option<(Json, Json)>,
    pub view: Option<Arc<TranscriptView>>,
    pub earlier: Option<(u64, Load)>,
    pub filling: HashMap<(i64, i64), (u64, Load)>,
    pub loads: u64,
}

pub(super) fn id(value: i64) -> EntryId {
    EntryId(value.max(0) as u64)
}

fn number(id: EntryId) -> i64 {
    id.get() as i64
}

/// Record that every entry with an ID in (`lo`, `hi`] is known, `added` being those entries in ID order: hold the new
/// ones, shrink the gaps they fill, extend the known range, and open a gap where the range does not continue it.
pub(super) fn cover(state: &mut State, lo: i64, hi: i64, added: Vec<EntryRecord>) {
    let first_added = added.first().map(|entry| number(entry.id));
    let fresh: Vec<EntryRecord> = added.into_iter().filter(|entry| state.ids.insert(entry.id)).collect();
    let held_empty = state.entries.is_empty();
    let newest_held = state.entries.last().map(|entry| entry.id);
    let appended = fresh.first().is_none_or(|first| newest_held.is_none_or(|newest| first.id > newest));
    let added_count = fresh.len();
    state.entries.extend(fresh);
    if !appended {
        state.entries.sort_by_key(|entry| entry.id);
    }
    let mut rebuild = false;
    if !state.started || (held_empty && state.gaps.is_empty() && lo >= state.tail) {
        // The first range, or one after an empty transcript: it is all the client knows.
        rebuild = !state.started || lo != state.low;
        state.started = true;
        state.low = lo;
        state.tail = state.tail.max(hi);
    } else if lo > state.tail {
        // Past the newest known ID: what lies between is missing.
        state.gaps.push((state.tail, first_added.unwrap_or(lo + 1)));
        state.tail = hi;
        rebuild = true;
    } else if hi != NONE && hi < state.low {
        // Below the oldest known ID: what lies between is missing.
        let next = state.entries.iter().map(|entry| number(entry.id)).find(|other| *other > hi);
        state.gaps.insert(0, (hi, next.unwrap_or(state.low + 1)));
        state.low = lo;
        rebuild = true;
    } else {
        if let Some(gaps) = subtract(state, lo, hi) {
            state.gaps = gaps;
            rebuild = true;
        }
        if lo < state.low {
            state.low = lo;
            rebuild = true;
        }
        state.tail = state.tail.max(hi);
    }
    if rebuild || !appended {
        state.items = Arc::new(build(state));
    } else if added_count > 0 {
        let mut items = state.items.as_ref().clone();
        let start = state.entries.len() - added_count;
        items.extend(state.entries[start..].iter().cloned().map(TranscriptItem::Entry));
        state.items = Arc::new(items);
    }
}

/// The gaps without (`lo`, `hi`]; `None` when none changes, as when the range is empty.
fn subtract(state: &State, lo: i64, hi: i64) -> Option<Vec<(i64, i64)>> {
    if hi <= lo {
        return None;
    }
    let mut changed = false;
    let mut gaps = Vec::new();
    for &(after, up_to) in &state.gaps {
        // Missing: IDs in (after, up_to); known now: IDs in (lo, hi].
        if hi <= after || lo.saturating_add(1) >= up_to {
            gaps.push((after, up_to));
            continue;
        }
        changed = true;
        if lo > after {
            let known = state.entries.iter().map(|entry| number(entry.id)).find(|other| *other > lo);
            gaps.push((after, up_to.min(known.unwrap_or(lo + 1))));
        }
        if hi + 1 < up_to {
            gaps.push((hi, up_to));
        }
    }
    changed.then_some(gaps)
}

/// Items in ID order: `Earlier` until the start is known, then entries with each gap before the entry after it.
fn build(state: &State) -> Vec<TranscriptItem> {
    let mut items = if state.low == NONE { Vec::new() } else { vec![TranscriptItem::Earlier] };
    let gap = |&(after, up_to): &(i64, i64)| TranscriptItem::Gap { after: id(after), up_to: id(up_to) };
    let mut gaps = state.gaps.iter().peekable();
    for entry in &state.entries {
        while let Some(next) = gaps.next_if(|(after, _)| *after < number(entry.id)) {
            items.push(gap(next));
        }
        items.push(TranscriptItem::Entry(entry.clone()));
    }
    items.extend(gaps.map(gap));
    items
}

/// The view with the current items; the previous one when nothing changed.
pub(super) fn result(state: &mut State) -> Arc<TranscriptView> {
    let (conversation, docs) = state.last.clone().unwrap_or((Json::Null, Json::empty_object()));
    if let Some(previous) = &state.view
        && Arc::ptr_eq(&previous.items, &state.items)
        && previous.docs == docs
        && previous.conversation == conversation
    {
        return previous.clone();
    }
    let view = Arc::new(TranscriptView { conversation, docs, items: state.items.clone() });
    state.view = Some(view.clone());
    view
}
