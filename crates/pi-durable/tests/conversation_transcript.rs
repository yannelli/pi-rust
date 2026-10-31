//! Port of upstream `test/conversation-transcript.test.ts`: `ConversationTranscript`, the client-side layer that
//! merges view windows into one transcript. This file holds the helpers and the merge cases; `loads.rs` the rest.

mod support;

#[path = "conversation_transcript/loads.rs"]
mod loads;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use pi_chord::{Context, Json, JsonMap};
use pi_durable::harness::*;
use pi_durable::{ConversationId, DurableResult, EntryId, EntryRecord, Page, ScanOrder};
use serde_json::{Value, json};
use support::Deferred;
use support::harness_support::ctx;

/// Entries with IDs that have holes: every third ID is missing.
fn transcript(count: usize) -> Vec<EntryRecord> {
    let ids = (1u64..).filter(|id| id % 3 != 0).take(count);
    ids.map(|id| EntryRecord {
        kind: "note".into(),
        model: None,
        data: None,
        edits: None,
        id: EntryId(id),
        conversation_id: ConversationId(1),
        head: None,
        by_task_id: None,
    })
    .collect()
}

/// A page of `entries` with `Conversation::entries()` semantics.
fn page(entries: &[EntryRecord], range: &EntryRange, limit: usize) -> Page<EntryRecord> {
    let matching = entries.iter().filter(|entry| {
        range.min_entry_id.is_none_or(|min| entry.id >= min) && range.max_entry_id.is_none_or(|max| entry.id <= max)
    });
    let mut ordered: Vec<EntryRecord> = matching.cloned().collect();
    if range.order != Some(ScanOrder::Ascending) {
        ordered.reverse();
    }
    let next = (ordered.len() > limit).then(JsonMap::new);
    ordered.truncate(limit);
    Page::new(ordered, next)
}

/// A loader over `entries` that waits for `gate`, when given, and counts its calls.
fn gated_loader(entries: Vec<EntryRecord>, gate: Option<Deferred>) -> (EntryLoader, Arc<AtomicUsize>) {
    let (entries, calls) = (Arc::new(entries), Arc::new(AtomicUsize::new(0)));
    let counted = calls.clone();
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, _: Context| {
        let (entries, gate, counted) = (entries.clone(), gate.clone(), counted.clone());
        async move {
            if let Some(gate) = gate {
                gate.wait().await;
            }
            counted.fetch_add(1, Ordering::SeqCst);
            DurableResult::Ok(page(&entries, &range, limit))
        }
        .boxed()
    });
    (load, calls)
}

fn loader(entries: &[EntryRecord]) -> (EntryLoader, Arc<AtomicUsize>) {
    gated_loader(entries.to_vec(), None)
}

fn calls(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::SeqCst)
}

/// The view of a window: entries `first..=last` of `all` with the entry before them.
fn window_of(all: &[EntryRecord], first: usize, last: usize, docs: Value) -> Json {
    let mut view = json!({ "conversation": { "id": 1 }, "entries": all[first..=last] });
    if first > 0 {
        view["before"] = json!(all[first - 1].id);
    }
    view["docs"] = docs;
    Json::from(view)
}

fn empty_window(before: Option<EntryId>) -> Json {
    let mut view = json!({ "conversation": { "id": 1 }, "entries": [] });
    if let Some(before) = before {
        view["before"] = json!(before);
    }
    view["docs"] = json!({});
    Json::from(view)
}

/// Items as a compact list: entry IDs, `gap after..up_to`, and `earlier`.
fn shape(view: &TranscriptView) -> Vec<Value> {
    let item = |item: &TranscriptItem| match item {
        TranscriptItem::Entry(entry) => json!(entry.id),
        TranscriptItem::Gap { after, up_to } => json!(format!("gap {after}..{up_to}")),
        TranscriptItem::Earlier => json!("earlier"),
    };
    view.items.iter().map(item).collect()
}

fn ids(entries: &[EntryRecord]) -> Vec<Value> {
    entries.iter().map(|entry| json!(entry.id)).collect()
}

fn with_earlier(entries: &[EntryRecord]) -> Vec<Value> {
    [vec![json!("earlier")], ids(entries)].concat()
}

fn gap(after: EntryId, up_to: EntryId) -> Value {
    json!(format!("gap {after}..{up_to}"))
}

fn gap_of(view: &TranscriptView) -> Option<TranscriptItem> {
    view.items.iter().find(|item| matches!(item, TranscriptItem::Gap { .. })).cloned()
}

fn has_gap(view: &TranscriptView) -> bool {
    gap_of(view).is_some()
}

fn helper(load: EntryLoader, page_size: Option<usize>) -> ConversationTranscript {
    ConversationTranscript::new(load, page_size).unwrap()
}

#[test]
fn shows_a_first_window_under_an_earlier_marker_or_alone_when_it_starts_the_transcript() {
    let all = transcript(20);
    let later = helper(loader(&all).0, None);
    assert_eq!(shape(&later.merge(&window_of(&all, 15, 19, json!({}))).unwrap()), with_earlier(&all[15..]));
    assert_eq!(later.newest(), Some(all[19].id));
    let whole = helper(loader(&all).0, None);
    assert_eq!(shape(&whole.merge(&window_of(&all, 0, 19, json!({}))).unwrap()), ids(&all));
}

#[test]
fn appends_keeps_entries_the_window_dropped_and_returns_the_same_items_when_nothing_is_added() {
    let all = transcript(30);
    let transcript = helper(loader(&all).0, None);
    let docs = json!({ "a": { "n": 1 } });
    let first = transcript.merge(&window_of(&all, 10, 19, docs.clone())).unwrap();
    let grown = transcript.merge(&window_of(&all, 10, 21, docs.clone())).unwrap();
    assert_eq!(shape(&grown), with_earlier(&all[10..22]));
    // The window dropped its oldest; the transcript keeps them.
    let spliced = transcript.merge(&window_of(&all, 18, 21, docs)).unwrap();
    assert!(Arc::ptr_eq(&spliced.items, &grown.items));
    assert!(Arc::ptr_eq(&spliced, &grown));
    // A docs-only change: a new view, the same items.
    let changed = window_of(&all, 18, 21, json!({ "a": { "n": 2 } }));
    let streamed = transcript.merge(&changed).unwrap();
    assert!(!Arc::ptr_eq(&streamed, &grown));
    assert!(Arc::ptr_eq(&streamed.items, &grown.items));
    assert!(streamed.docs.same_ref(changed.get("docs").unwrap()));
    assert!(!Arc::ptr_eq(&first.items, &grown.items));
}

#[tokio::test]
async fn opens_a_gap_when_a_window_does_not_continue_the_transcript_and_fills_it_page_by_page() {
    let all = transcript(200);
    let (load, counter) = loader(&all);
    let transcript = helper(load, Some(50));
    transcript.merge(&window_of(&all, 150, 159, json!({}))).unwrap();
    // A stall: the next window starts well past the newest entry held.
    let stalled = transcript.merge(&window_of(&all, 190, 199, json!({}))).unwrap();
    let missing = gap_of(&stalled).unwrap();
    assert_eq!(missing, TranscriptItem::Gap { after: all[159].id, up_to: all[190].id });
    let expected = [with_earlier(&all[150..160]), vec![gap(all[159].id, all[190].id)], ids(&all[190..])].concat();
    assert_eq!(shape(&stalled), expected);
    // 30 entries are missing; one page loads them all and closes the gap.
    let filled = transcript.fill_gap(&missing, &ctx()).await.unwrap();
    assert_eq!(shape(&filled), with_earlier(&all[150..]));
    assert_eq!(calls(&counter), 1);
}

#[tokio::test]
async fn shrinks_a_gap_from_its_start_while_more_than_a_page_is_missing() {
    let all = transcript(200);
    let transcript = helper(loader(&all).0, Some(10));
    transcript.merge(&window_of(&all, 100, 109, json!({}))).unwrap();
    let mut view = transcript.merge(&window_of(&all, 190, 199, json!({}))).unwrap();
    view = transcript.fill_gap(&gap_of(&view).unwrap(), &ctx()).await.unwrap();
    assert_eq!(gap_of(&view), Some(TranscriptItem::Gap { after: all[119].id, up_to: all[190].id }));
    while let Some(next) = gap_of(&view) {
        view = transcript.fill_gap(&next, &ctx()).await.unwrap();
    }
    assert_eq!(shape(&view), with_earlier(&all[100..]));
}

#[tokio::test]
async fn loads_earlier_pages_until_the_start_then_drops_the_marker() {
    let all = transcript(45);
    let (load, counter) = loader(&all);
    let transcript = helper(load, Some(20));
    transcript.merge(&window_of(&all, 40, 44, json!({}))).unwrap();
    let view = transcript.load_earlier(&ctx()).await.unwrap();
    assert_eq!(shape(&view), with_earlier(&all[20..]));
    // A page without a next cursor reaches the start.
    let view = transcript.load_earlier(&ctx()).await.unwrap();
    assert_eq!(shape(&view), ids(&all));
    let before = calls(&counter);
    assert!(Arc::ptr_eq(&transcript.load_earlier(&ctx()).await.unwrap(), &view));
    assert_eq!(calls(&counter), before);
}

#[tokio::test]
async fn shares_one_load_between_concurrent_calls_of_load_earlier_and_of_fill_gap_for_one_gap() {
    let all = transcript(100);
    let gate = Deferred::default();
    let (load, counter) = gated_loader(all.clone(), Some(gate.clone()));
    let transcript = helper(load, None);
    transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
    let stalled = transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap();
    let missing = gap_of(&stalled).unwrap();
    let earlier = [transcript.load_earlier(&ctx()), transcript.load_earlier(&ctx())];
    let fills = [transcript.fill_gap(&missing, &ctx()), transcript.fill_gap(&missing, &ctx())];
    gate.resolve();
    let loaded = futures::future::join_all(earlier.into_iter().chain(fills)).await;
    assert!(Arc::ptr_eq(loaded[0].as_ref().unwrap(), loaded[1].as_ref().unwrap()));
    assert!(Arc::ptr_eq(loaded[2].as_ref().unwrap(), loaded[3].as_ref().unwrap()));
    assert_eq!(calls(&counter), 2);
    assert_eq!(shape(&transcript.load_earlier(&ctx()).await.unwrap()), ids(&all));
}

#[test]
fn resumes_after_newest_an_empty_window_right_after_it_adds_nothing_one_past_it_opens_a_gap() {
    let all = transcript(30);
    let transcript = helper(loader(&all).0, None);
    let view = transcript.merge(&window_of(&all, 10, 19, json!({}))).unwrap();
    let resumed = transcript.merge(&empty_window(transcript.newest())).unwrap();
    assert!(Arc::ptr_eq(&resumed.items, &view.items));
    // A window that is empty but starts past `newest`: the entries up to its `before` are missing.
    let past = transcript.merge(&empty_window(Some(all[25].id))).unwrap();
    let opened = gap(all[19].id, EntryId(all[25].id.get() + 1));
    assert_eq!(shape(&past).last(), Some(&opened));
    assert_eq!(transcript.newest(), Some(all[25].id));
    // The next window continues it.
    let next = shape(&transcript.merge(&window_of(&all, 26, 27, json!({}))).unwrap());
    assert_eq!(next[next.len() - 3..], [opened, json!(all[26].id), json!(all[27].id)]);
}

#[test]
fn fills_a_gap_from_a_window_that_covers_it() {
    let all = transcript(100);
    let transcript = helper(loader(&all).0, None);
    transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
    transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap();
    // A wider window, such as a new watch with a larger `max`, covers the second half of the gap.
    let view = transcript.merge(&window_of(&all, 70, 99, json!({}))).unwrap();
    let expected = [with_earlier(&all[40..50]), vec![gap(all[49].id, all[70].id)], ids(&all[70..])].concat();
    assert_eq!(shape(&view), expected);
}

#[tokio::test]
async fn rejects_load_earlier_before_the_first_merge_and_returns_the_view_for_a_gap_that_is_gone() {
    let all = transcript(10);
    let transcript = helper(loader(&all).0, None);
    let error = transcript.load_earlier(&ctx()).await.err().unwrap();
    assert!(error.to_string().contains("merge a view first"), "{error}");
    let view = transcript.merge(&window_of(&all, 0, 9, json!({}))).unwrap();
    let stale = TranscriptItem::Gap { after: EntryId(1), up_to: EntryId(2) };
    assert!(Arc::ptr_eq(&transcript.fill_gap(&stale, &ctx()).await.unwrap(), &view));
}
