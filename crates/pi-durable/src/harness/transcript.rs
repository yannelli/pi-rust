//! `ConversationTranscript` (spec section 9.3): what a client has of one conversation, merged from view windows and
//! pages it loaded, with the gaps between (upstream `harness/transcript.ts`).

use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use pi_chord::{Context, Json};

use super::conversation::EntryRange;
use super::transcript_state::{NONE, State, cover, id, result};
use super::util::lock;
use super::view_window::ConversationWindow;
use crate::errors::{DurableError, DurableResult};
use crate::ids::EntryId;
use crate::storage::scan::MAX_SAFE_INTEGER;
use crate::types::{EntryRecord, Page, ScanOrder};

/// One row of a client's transcript.
#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    Entry(EntryRecord),
    /// Entries strictly between `after` and `up_to` are missing: render "load more" and call `fill_gap()`.
    Gap {
        after: EntryId,
        up_to: EntryId,
    },
    /// The transcript continues above: render "load earlier" and call `load_earlier()`. Always the first item.
    Earlier,
}

/// The conversation view as a renderer sees it: `items`, the client's whole transcript, instead of the window.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptView {
    pub conversation: Json,
    pub docs: Json,
    pub items: Arc<Vec<TranscriptItem>>,
}

/// Pages a conversation's entries, as `Conversation::entries()` does without a cursor.
pub type EntryLoader =
    Arc<dyn Fn(EntryRange, usize, Context) -> BoxFuture<'static, DurableResult<Page<EntryRecord>>> + Send + Sync>;

type ViewFuture = BoxFuture<'static, DurableResult<Arc<TranscriptView>>>;

/// A layer over any view: `merge()` every value; the transcript only grows, and a window that does not continue it
/// leaves a gap. Concurrent loads of one range share one loader call and hold the state weakly.
pub struct ConversationTranscript {
    load: EntryLoader,
    page_size: usize,
    state: Arc<Mutex<State>>,
}

fn ready(value: DurableResult<Arc<TranscriptView>>) -> ViewFuture {
    futures::future::ready(value).boxed()
}

fn merge_first() -> DurableError {
    DurableError::error("ConversationTranscript: merge a view first")
}

fn dropped() -> DurableError {
    DurableError::error("ConversationTranscript was dropped")
}

/// `id` as the transcript's number, an error past the IDs a Session mints.
fn safe(id: u64) -> DurableResult<i64> {
    if id > MAX_SAFE_INTEGER {
        return Err(DurableError::error(format!("Entry ID must be at most {MAX_SAFE_INTEGER}, got {id}")));
    }
    Ok(id as i64)
}

fn safe_entries(entries: &[EntryRecord]) -> DurableResult<()> {
    entries.iter().try_for_each(|entry| safe(entry.id.get()).map(|_| ()))
}

impl ConversationTranscript {
    /// `page_size`: entries `load_earlier()` and `fill_gap()` load per call; default 50, at least 1.
    pub fn new(load: EntryLoader, page_size: Option<usize>) -> DurableResult<Self> {
        let page_size = page_size.unwrap_or(50);
        if page_size < 1 {
            return Err(DurableError::error(format!("Page size must be an integer >= 1, got {page_size}")));
        }
        let state = State { low: NONE, tail: NONE, ..State::default() };
        Ok(Self { load, page_size, state: Arc::new(Mutex::new(state)) })
    }

    /// The newest entry the transcript reaches, or a later empty window's `before`; `None` before the first merge
    /// and for an empty transcript.
    pub fn newest(&self) -> Option<EntryId> {
        let tail = lock(&self.state).tail;
        (tail != NONE).then(|| id(tail))
    }

    /// The window that resumes this transcript after a drop: `from` right after `newest`, the default window for an
    /// empty transcript, and `None` before the first merge.
    pub fn resume(&self) -> Option<ConversationWindow> {
        if !lock(&self.state).started {
            return None;
        }
        let from = self.newest().map(|newest| EntryId(newest.get() + 1));
        Some(ConversationWindow { from, ..ConversationWindow::default() })
    }

    /// Take one value of the view: its window's entries join the transcript.
    pub fn merge(&self, view: &Json) -> DurableResult<Arc<TranscriptView>> {
        let entries: Vec<EntryRecord> = match view.get("entries") {
            Some(entries) => entries.deserialize_into()?,
            None => Vec::new(),
        };
        safe_entries(&entries)?;
        let before = view
            .get("before")
            .map(|before| {
                before
                    .as_u64()
                    .ok_or_else(|| DurableError::error("Entry ID must be a nonnegative integer"))
                    .and_then(safe)
            })
            .transpose()?;
        let conversation = view.get("conversation").cloned().unwrap_or(Json::Null);
        let docs = view.get("docs").cloned().unwrap_or_else(Json::empty_object);
        let mut state = lock(&self.state);
        state.last = Some((conversation, docs));
        let hi = entries.last().map(|entry| entry.id.get() as i64).or(before).unwrap_or(NONE);
        cover(&mut state, before.unwrap_or(NONE), hi, entries);
        Ok(result(&mut state))
    }

    /// Load the page before the oldest entry held; without anything earlier, the current view.
    pub fn load_earlier(&self, context: &Context) -> ViewFuture {
        let mut state = lock(&self.state);
        if state.last.is_none() {
            return ready(Err(merge_first()));
        }
        if state.low == NONE {
            return ready(Ok(result(&mut state)));
        }
        if let Some((_, load)) = &state.earlier {
            return load.clone().boxed();
        }
        state.loads += 1;
        let (low, key) = (state.low, state.loads);
        let order = Some(ScanOrder::Descending);
        let range = EntryRange { max_entry_id: Some(id(low)), order, ..EntryRange::default() };
        let (loader, size, shared, context) =
            (self.load.clone(), self.page_size, Arc::downgrade(&self.state), context.clone());
        let load = async move {
            let loaded = loader(range, size, context).await;
            let shared = shared.upgrade().ok_or_else(dropped)?;
            let mut state = lock(&shared);
            if state.earlier.as_ref().is_some_and(|(running, _)| *running == key) {
                state.earlier = None;
            }
            let page = loaded?;
            safe_entries(&page.items)?;
            let ended = page.next.is_none() || page.items.is_empty();
            let mut items = page.items;
            items.reverse();
            let lo = match items.first() {
                Some(oldest) if !ended => oldest.id.get() as i64 - 1,
                _ => NONE,
            };
            cover(&mut state, lo, low, items);
            Ok(result(&mut state))
        };
        let load = load.boxed().shared();
        state.earlier = Some((key, load.clone()));
        load.boxed()
    }

    /// Load the next page of `gap`, shrinking it from its start, or removing it once nothing is missing. A gap that
    /// shrank since loads the gap it became; one that is gone returns the current view.
    pub fn fill_gap(&self, gap: &TranscriptItem, context: &Context) -> ViewFuture {
        let TranscriptItem::Gap { after, up_to } = gap else {
            return ready(Err(DurableError::type_error("fill_gap takes a gap item")));
        };
        // A gap past the safe range matches no gap the transcript holds.
        let number = |id: &EntryId| i64::try_from(id.get()).unwrap_or(i64::MAX);
        let (after, up_to) = (number(after), number(up_to));
        let mut state = lock(&self.state);
        let exact = state.gaps.iter().copied().find(|gap| *gap == (after, up_to));
        let current = exact.or_else(|| state.gaps.iter().copied().find(|(a, u)| *a < up_to && after < *u));
        let Some((after, up_to)) = current else {
            if state.last.is_none() {
                return ready(Err(merge_first()));
            }
            return ready(Ok(result(&mut state)));
        };
        // Keyed by the gap the call resolved to, so a stale descriptor and the current one share the load.
        if let Some((_, load)) = state.filling.get(&(after, up_to)) {
            return load.clone().boxed();
        }
        state.loads += 1;
        let key = state.loads;
        let (min_entry_id, max_entry_id) = (Some(id(after + 1)), Some(id(up_to - 1)));
        let range = EntryRange { min_entry_id, max_entry_id, order: Some(ScanOrder::Ascending) };
        let (loader, size, shared, context) =
            (self.load.clone(), self.page_size, Arc::downgrade(&self.state), context.clone());
        let load = async move {
            let loaded = loader(range, size, context).await;
            let shared = shared.upgrade().ok_or_else(dropped)?;
            let mut state = lock(&shared);
            if state.filling.get(&(after, up_to)).is_some_and(|(running, _)| *running == key) {
                state.filling.remove(&(after, up_to));
            }
            let page = loaded?;
            safe_entries(&page.items)?;
            let hi = match page.items.last() {
                Some(newest) if page.next.is_some() => newest.id.get() as i64,
                _ => up_to - 1,
            };
            cover(&mut state, after, hi, page.items);
            Ok(result(&mut state))
        };
        let load = load.boxed().shared();
        state.filling.insert((after, up_to), (key, load.clone()));
        load.boxed()
    }
}
