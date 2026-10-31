//! Upstream `conversation-transcript.test.ts`, "ConversationTranscript loads" and "over a Harness".

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::support::harness_support::{all_entries, chat_setup, open_chat};
use pi_durable::DurableError;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;

use super::*;
use crate::support::{ValueCall, on_value};

/// A loader over `entries` that fails with `message` while `failing` is set, counting every call.
fn failing_loader(
    entries: &[EntryRecord],
    failing: Arc<AtomicBool>,
    message: &'static str,
) -> (EntryLoader, Arc<AtomicUsize>) {
    let (inner, _) = loader(entries);
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
        counted.fetch_add(1, Ordering::SeqCst);
        if failing.load(Ordering::SeqCst) {
            return futures::future::ready(Err(DurableError::error(message))).boxed();
        }
        inner(range, limit, context)
    });
    (load, calls)
}

#[tokio::test]
async fn retries_a_load_whose_loader_failed_for_load_earlier_and_fill_gap() {
    let all = transcript(100);
    let offline = Arc::new(AtomicBool::new(true));
    let (load, counter) = failing_loader(&all, offline.clone(), "temporarily offline");
    let transcript = helper(load, None);
    transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
    let stalled = transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap();
    let missing = gap_of(&stalled).unwrap();
    let error = transcript.load_earlier(&ctx()).await.err().unwrap();
    assert!(error.to_string().contains("temporarily offline"), "{error}");
    let error = transcript.fill_gap(&missing, &ctx()).await.err().unwrap();
    assert!(error.to_string().contains("temporarily offline"), "{error}");
    offline.store(false, Ordering::SeqCst);
    let expected = [ids(&all[..50]), vec![gap(all[49].id, all[90].id)], ids(&all[90..])].concat();
    assert_eq!(shape(&transcript.load_earlier(&ctx()).await.unwrap()), expected);
    assert_eq!(shape(&transcript.fill_gap(&missing, &ctx()).await.unwrap()), ids(&all));
    assert_eq!(calls(&counter), 4);
}

#[tokio::test]
async fn retries_a_load_that_rejected() {
    let all = transcript(30);
    let fail = Arc::new(AtomicBool::new(true));
    let transcript = helper(failing_loader(&all, fail.clone(), "timeout").0, None);
    transcript.merge(&window_of(&all, 20, 29, json!({}))).unwrap();
    assert!(transcript.load_earlier(&ctx()).await.err().unwrap().to_string().contains("timeout"));
    fail.store(false, Ordering::SeqCst);
    assert_eq!(shape(&transcript.load_earlier(&ctx()).await.unwrap()), ids(&all));
}

#[test]
fn keeps_one_gap_for_an_empty_window_whose_before_lies_inside_it() {
    let all = transcript(100);
    let transcript = helper(loader(&all).0, None);
    transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
    let before = transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap();
    let view = transcript.merge(&empty_window(Some(all[65].id))).unwrap();
    assert!(Arc::ptr_eq(&view.items, &before.items));
    assert_eq!(view.items.iter().filter(|item| matches!(item, TranscriptItem::Gap { .. })).count(), 1);
}

#[tokio::test]
async fn shares_one_load_between_a_stale_gap_descriptor_and_the_gap_it_shrank_into() {
    let all = transcript(100);
    let gate = Deferred::default();
    let (load, counter) = gated_loader(all.clone(), Some(gate.clone()));
    let transcript = helper(load, Some(10));
    transcript.merge(&window_of(&all, 10, 19, json!({}))).unwrap();
    let stale = gap_of(&transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap()).unwrap();
    gate.resolve();
    let current = gap_of(&transcript.fill_gap(&stale, &ctx()).await.unwrap()).unwrap();
    assert_ne!(current, stale);
    let loads = calls(&counter);
    let (a, b) = (transcript.fill_gap(&stale, &ctx()), transcript.fill_gap(&current, &ctx()));
    let (a, b) = futures::join!(a, b);
    assert!(Arc::ptr_eq(&a.unwrap(), &b.unwrap()));
    assert_eq!(calls(&counter), loads + 1);
}

#[tokio::test]
async fn treats_an_empty_page_with_a_next_cursor_as_the_end_of_the_range() {
    let all = transcript(60);
    // A remote loader that always returns a cursor.
    let (inner, _) = loader(&all);
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
        let page = inner(range, limit, context);
        async move { page.await.map(|page| Page::new(page.items, Some(JsonMap::new()))) }.boxed()
    });
    let transcript = helper(load, Some(100));
    transcript.merge(&window_of(&all, 10, 19, json!({}))).unwrap();
    let mut view = transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
    view = transcript.fill_gap(&gap_of(&view).unwrap(), &ctx()).await.unwrap();
    // The first page held every missing entry; the next one is empty and closes the gap.
    view = transcript.fill_gap(&gap_of(&view).unwrap(), &ctx()).await.unwrap();
    assert!(!has_gap(&view));
    transcript.load_earlier(&ctx()).await.unwrap();
    let view = transcript.load_earlier(&ctx()).await.unwrap();
    assert_eq!(shape(&view), ids(&all[..50]));
}

#[test]
fn resumes_with_resume_none_before_a_merge_the_default_for_an_empty_transcript_from_after_the_newest() {
    let all = transcript(30);
    let transcript = helper(loader(&all).0, None);
    assert_eq!(transcript.resume(), None);
    transcript.merge(&empty_window(None)).unwrap();
    assert_eq!(transcript.resume(), Some(ConversationWindow::default()));
    transcript.merge(&window_of(&all, 0, 9, json!({}))).unwrap();
    let from = Some(EntryId(all[9].id.get() + 1));
    assert_eq!(transcript.resume(), Some(ConversationWindow { from, ..ConversationWindow::default() }));
}

#[test]
fn rejects_a_page_size_below_one() {
    let error = ConversationTranscript::new(loader(&transcript(1)).0, Some(0)).err().unwrap();
    assert!(error.to_string().contains("Page size must be an integer >= 1, got 0"), "{error}");
}

fn entry_ids(view: &TranscriptView) -> Vec<Value> {
    let entries = view.items.iter().filter_map(|item| match item {
        TranscriptItem::Entry(entry) => Some(json!(entry.id)),
        _ => None,
    });
    entries.collect()
}

async fn notes(root: &Conversation, count: usize) {
    for _ in 0..count {
        let id = root.id;
        let note = move |tx: Tx| async move { tx.append_entry(id, pi_durable::EntryDraft::new("note")).await };
        root.commit(note, &ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn follows_the_walkthrough_live_appends_a_stall_fill_gap_load_earlier_and_a_resume_with_from() {
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await;
    notes(&root, 120).await;
    let pages = root.clone();
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
        let pages = pages.clone();
        async move { pages.entries(range, limit, None, &context).await }.boxed()
    });
    let transcript = Arc::new(helper(load, None));
    let window = ConversationWindow { min: Some(10), max: Some(20), from: None };
    let watch = root.watch(&ctx(), Some(window)).await.unwrap();
    let latest = Arc::new(Mutex::new(transcript.merge(&watch.value()).unwrap()));
    let (merging, sink) = (transcript.clone(), latest.clone());
    let listen = on_value(move |ValueCall { value, .. }, _| {
        let merged = merging.merge(&value).map(|view| *sink.lock().unwrap() = view);
        async move { merged }
    });
    watch.start(listen).unwrap();
    notes(&root, 15).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let view = latest.lock().unwrap().clone();
    assert!(!has_gap(&view));
    assert_eq!(entry_ids(&view).len(), 25);
    // A drop: the client misses 40 entries, more than `max`, and resumes after the newest it has.
    watch.stop().await;
    notes(&root, 40).await;
    let resume =
        |transcript: &ConversationTranscript| ConversationWindow { from: transcript.resume().unwrap().from, ..window };
    let resumed = root.watch(&ctx(), Some(resume(&transcript))).await.unwrap();
    let mut view = transcript.merge(&resumed.value()).unwrap();
    assert_eq!(view.items.iter().filter(|item| matches!(item, TranscriptItem::Gap { .. })).count(), 1);
    while let Some(missing) = gap_of(&view) {
        view = transcript.fill_gap(&missing, &ctx()).await.unwrap();
    }
    while view.items.first() == Some(&TranscriptItem::Earlier) {
        view = transcript.load_earlier(&ctx()).await.unwrap();
    }
    let all: Vec<Value> = all_entries(&root).await.iter().map(|entry| json!(entry.id)).collect();
    assert_eq!(entry_ids(&view), all);
    // A short drop resumes without a gap.
    notes(&root, 3).await;
    let again = root.watch(&ctx(), Some(resume(&transcript))).await.unwrap();
    assert_eq!(again.value().get("entries").and_then(Json::as_array).map(Vec::len), Some(3));
    let view = transcript.merge(&again.value()).unwrap();
    assert!(view.items.iter().all(|item| matches!(item, TranscriptItem::Entry(_))));
    resumed.stop().await;
    again.stop().await;
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_entry_ids_past_the_safe_integer_range_from_a_view_or_a_loader() {
    let all = transcript(3);
    let mut unsafe_entry = all[0].clone();
    unsafe_entry.id = EntryId(pi_durable::storage::scan::MAX_SAFE_INTEGER + 1);
    let returned = unsafe_entry.clone();
    let load: EntryLoader = Arc::new(move |_: EntryRange, _: usize, _: Context| {
        futures::future::ready(Ok(Page::new(vec![returned.clone()], None))).boxed()
    });
    let transcript = helper(load, None);
    let view = Json::from(json!({ "conversation": { "id": 1 }, "entries": [unsafe_entry], "docs": {} }));
    let error = transcript.merge(&view).err().unwrap();
    assert!(error.to_string().contains("Entry ID must be at most 9007199254740991"), "{error}");
    for before in [json!(u64::MAX), json!(-1), json!(1.5), json!(null), json!("1")] {
        let view = Json::from(json!({ "conversation": { "id": 1 }, "entries": [], "before": before, "docs": {} }));
        assert!(transcript.merge(&view).is_err(), "{before}");
        assert_eq!(transcript.resume(), None, "an invalid marker must not start the transcript");
    }
    let view = transcript.merge(&window_of(&all, 1, 2, json!({}))).unwrap();
    // A gap descriptor past the safe range matches no gap.
    let past = TranscriptItem::Gap { after: EntryId(u64::MAX - 1), up_to: EntryId(u64::MAX) };
    assert!(Arc::ptr_eq(&transcript.fill_gap(&past, &ctx()).await.unwrap(), &view));
    let error = transcript.load_earlier(&ctx()).await.err().unwrap();
    assert!(error.to_string().contains("Entry ID must be at most"), "{error}");
}

#[tokio::test]
async fn drops_its_state_and_loader_with_an_abandoned_unpolled_or_pending_load() {
    let all = transcript(100);
    for poll_once in [false, true] {
        let held = Arc::new(());
        let (inner, _) = gated_loader(all.clone(), Some(Deferred::default()));
        let marker = held.clone();
        let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
            let _ = &marker;
            inner(range, limit, context)
        });
        let transcript = helper(load, None);
        transcript.merge(&window_of(&all, 40, 49, json!({}))).unwrap();
        let stalled = transcript.merge(&window_of(&all, 90, 99, json!({}))).unwrap();
        let mut earlier = transcript.load_earlier(&ctx());
        let mut fill = transcript.fill_gap(&gap_of(&stalled).unwrap(), &ctx());
        if poll_once {
            assert!(futures::poll!(&mut earlier).is_pending());
            assert!(futures::poll!(&mut fill).is_pending());
        }
        drop((earlier, fill));
        drop(transcript);
        assert_eq!(Arc::strong_count(&held), 1, "poll_once: {poll_once}");
    }
}
