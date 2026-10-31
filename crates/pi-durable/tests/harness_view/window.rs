//! Upstream `harness-view.test.ts`, "conversation view window": each observer's window over one mount.

use pi_durable::EntryRecord;
use pi_durable::session::{Observer, WatchCall, WatchEnd};

use super::*;
use crate::support::{ValueCall, on_value};

/// Append one entry of `kind` per commit; returns the entries.
async fn notes(conversation: &Conversation, count: usize, kind: &'static str) -> Vec<EntryRecord> {
    let mut entries = Vec::new();
    for index in 0..count {
        let id = conversation.id;
        let draft = EntryDraft { data: Some(Json::from(index as u64)), ..EntryDraft::new(kind) };
        entries.push(
            conversation
                .commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context())
                .await
                .unwrap(),
        );
    }
    entries
}

fn ids(entries: &[EntryRecord]) -> Vec<Value> {
    entries.iter().map(|entry| json!(entry.id)).collect()
}

fn view_ids(view: &Json) -> Vec<Value> {
    let entries = view.get("entries").unwrap().to_serde();
    entries.as_array().unwrap().iter().map(|entry| entry["id"].clone()).collect()
}

fn before(view: &Json) -> Option<Value> {
    view.get("before").map(Json::to_serde)
}

fn window(min: usize, max: Option<usize>) -> ConversationWindow {
    ConversationWindow { min: Some(min), max, from: None }
}

fn from(from: EntryId) -> ConversationWindow {
    ConversationWindow { from: Some(from), ..ConversationWindow::default() }
}

async fn open() -> (Harness, Conversation) {
    open_chat(Arc::new(MemoryStorage::new()), &chat_setup(Default::default())).await
}

#[tokio::test]
async fn starts_with_the_newest_min_entries_and_the_entry_before_them_or_the_whole_short_transcript() {
    let (harness, root) = open().await;
    let written = notes(&root, 10, "note").await;
    let view = fresh_window(&root, window(4, None)).await;
    assert_eq!(view_ids(&view), ids(&written[6..]));
    assert_eq!(before(&view), Some(json!(written[5].id)));
    // The default window: the newest 50, here all ten, starting the transcript.
    let all = fresh_window(&root, ConversationWindow::default()).await;
    assert_eq!(view_ids(&all), ids(&written));
    assert_eq!(before(&all), None);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn takes_appends_up_to_max_then_drops_its_oldest_in_one_splice_down_to_min() {
    let (harness, root) = open().await;
    let written = notes(&root, 4, "note").await;
    let (initial, frames, watch) = record_window(&root, Some(window(3, Some(5)))).await;
    assert_eq!(view_ids(&initial), ids(&written[1..]));
    let appended = notes(&root, 3, "note").await;
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let lengths: Vec<usize> = frames.iter().map(|(value, _)| view_ids(value).len()).collect();
    assert_eq!(lengths, [4, 5, 3]);
    let [e5, _, e7] = [&appended[0], &appended[1], &appended[2]].map(|entry| serde_json::to_value(entry).unwrap());
    assert_eq!(ops_json(&frames[0].1), [json!(["p", ["entries"], 3, 0, [e5]])]);
    assert_eq!(
        ops_json(&frames[2].1),
        [
            json!(["p", ["entries"], 5, 0, [e7]]),
            json!(["p", ["entries"], 0, 3, []]),
            json!(["s", ["before"], written[3].id])
        ]
    );
    assert_eq!(view_ids(&frames[2].0), ids(&appended));
    assert_eq!(before(&frames[2].0), Some(json!(written[3].id)));
    let expected = committed(&harness, &root, initial.get("conversation").unwrap(), 3).await;
    assert_eq!(Json::from(replay(&initial, &frames)), Json::from(expected));
    // Entries that stay keep their identity: a splice moves them and does not copy them.
    let entry = |frame: usize, index: usize| frames[frame].0.get("entries").unwrap().at(index).unwrap().clone();
    assert!(entry(2, 0).same_ref(&entry(1, 3)));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_the_newest_min_of_a_commit_that_appends_more_than_max_at_once() {
    let (harness, root) = open().await;
    notes(&root, 2, "note").await;
    let (initial, frames, watch) = record_window(&root, Some(window(2, Some(3)))).await;
    let id = root.id;
    let burst = move |tx: Tx| async move {
        let mut entries = Vec::new();
        for _ in 0..5 {
            entries.push(tx.append_entry(id, EntryDraft::new("burst")).await?);
        }
        Ok(entries)
    };
    let burst = root.commit(burst, &context()).await.unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert_eq!(view_ids(&frames[0].0), ids(&burst[3..]));
    assert_eq!(before(&frames[0].0), Some(json!(burst[2].id)));
    let expected = committed(&harness, &root, initial.get("conversation").unwrap(), 2).await;
    assert_eq!(Json::from(replay(&initial, &frames)), Json::from(expected));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn starts_at_from_when_the_entries_since_fit_in_max_otherwise_at_the_newest_min() {
    let (harness, root) = open().await;
    let written = notes(&root, 30, "note").await;
    let bounded = |at: EntryId| ConversationWindow { from: Some(at), ..window(5, Some(10)) };
    // Ten entries since `from`: all of them, with the entry before `from`.
    let within = fresh_window(&root, bounded(written[20].id)).await;
    assert_eq!(view_ids(&within), ids(&written[20..]));
    assert_eq!(before(&within), Some(json!(written[19].id)));
    // Fewer than `min` since `from`: only those.
    let short = fresh_window(&root, bounded(written[28].id)).await;
    assert_eq!(view_ids(&short), ids(&written[28..]));
    assert_eq!(before(&short), Some(json!(written[27].id)));
    // Resuming after the newest entry: an empty window right after it.
    let resumed = fresh_window(&root, bounded(EntryId(written[29].id.get() + 1))).await;
    assert!(view_ids(&resumed).is_empty());
    assert_eq!(before(&resumed), Some(json!(written[29].id)));
    // Eleven since `from`: more than `max`, so the newest `min`; `before` shows the client a gap.
    let beyond = fresh_window(&root, bounded(written[19].id)).await;
    assert_eq!(view_ids(&beyond), ids(&written[25..]));
    assert_eq!(before(&beyond), Some(json!(written[24].id)));
    // From the transcript's start.
    let whole = fresh_window(&root, ConversationWindow { from: Some(written[0].id), ..window(40, None) }).await;
    assert_eq!(view_ids(&whole), ids(&written));
    assert_eq!(before(&whole), None);
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn advances_a_resumed_window_by_appends_from_its_start() {
    let (harness, root) = open().await;
    let written = notes(&root, 5, "note").await;
    let (initial, frames, watch) = record_window(&root, Some(from(EntryId(written[4].id.get() + 1)))).await;
    let later = notes(&root, 2, "note").await;
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let last = &frames.last().unwrap().0;
    assert_eq!(view_ids(last), ids(&later));
    assert_eq!(before(last), Some(json!(written[4].id)));
    assert_eq!(Json::from(replay(&initial, &frames)), *last);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn uses_from_only_for_the_first_value_a_from_past_the_newest_entry_starts_empty_and_takes_every_append() {
    let (harness, root) = open().await;
    let written = notes(&root, 3, "note").await;
    let start = EntryId(written[2].id.get() + 1000);
    let (initial, frames, watch) = record_window(&root, Some(from(start))).await;
    assert!(view_ids(&initial).is_empty());
    assert_eq!(before(&initial), Some(json!(written[2].id)));
    // An entry below `from` is still appended: the window continues from its attachment.
    let next = notes(&root, 1, "note").await;
    assert!(next[0].id < start);
    drained().await;
    let frames = frames.lock().unwrap().clone();
    let last = &frames.last().unwrap().0;
    assert_eq!(view_ids(last), ids(&next));
    assert_eq!(before(last), Some(json!(written[2].id)));
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

struct Ignore;

impl Observer for Ignore {
    fn advance(&self, _call: &WatchCall, _context: &pi_chord::Context) -> pi_durable::DurableResult<()> {
        Ok(())
    }

    fn end(&self, _end: WatchEnd) {}
}

#[tokio::test]
async fn rejects_window_bounds_that_put_min_below_one_or_max_below_min() {
    let (harness, root) = open().await;
    let error = root.watch(&context(), Some(window(0, None))).await.err().unwrap();
    assert!(error.to_string().contains("Window min must be an integer >= 1, got 0"), "{error}");
    let error = root.watch(&context(), Some(window(10, Some(9)))).await.err().unwrap();
    assert!(error.to_string().contains("Window max must be an integer >= min (10), got 9"), "{error}");
    let zero = ConversationWindow { max: Some(0), ..ConversationWindow::default() };
    assert!(harness.observe_conversation(root.id, Arc::new(Ignore), &context(), Some(zero)).await.is_err());
    // `max` may equal `min`.
    root.watch(&context(), Some(window(3, Some(3)))).await.unwrap().stop().await;
    harness.close(&context()).await.unwrap();
}

/// Records every value it is given.
struct Values(Arc<Mutex<Vec<Json>>>);

impl Observer for Values {
    fn advance(&self, call: &WatchCall, _context: &pi_chord::Context) -> pi_durable::DurableResult<()> {
        if let WatchCall::Value { value, .. } = call {
            self.0.lock().unwrap().push(value.clone());
        }
        Ok(())
    }

    fn end(&self, _end: WatchEnd) {}
}

#[tokio::test]
async fn gives_each_observer_its_own_window_over_one_mount() {
    let (harness, root) = open().await;
    let written = notes(&root, 6, "note").await;
    let changes: Arc<Mutex<Vec<Json>>> = Arc::default();
    let observer = Arc::new(Values(changes.clone()));
    let observation =
        harness.observe_conversation(root.id, observer, &context(), Some(window(2, Some(3)))).await.unwrap();
    let (wide, frames, watch) = record_window(&root, Some(window(5, None))).await;
    assert_eq!(view_ids(&observation.value), ids(&written[4..]));
    assert_eq!(view_ids(&wide), ids(&written[1..]));
    let next = notes(&root, 1, "note").await;
    drained().await;
    let changes = changes.lock().unwrap().clone();
    let frames = frames.lock().unwrap().clone();
    assert_eq!(view_ids(&changes[0]), ids(&[&written[4..], &next[..]].concat()));
    assert_eq!(view_ids(&frames[0].0), ids(&[&written[1..], &next[..]].concat()));
    assert!(changes[0].get("docs").unwrap().same_ref(frames[0].0.get("docs").unwrap()));
    observation.detach();
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn bounds_an_overflow_replacement_by_max() {
    let (harness, root) = open().await;
    let watch = root.watch(&context(), Some(window(5, Some(6)))).await.unwrap();
    // Past 10,000 pending operations, the next call is one root replacement: here an append per commit, and a
    // splice and a `before` every other commit.
    notes(&root, 6000, "note").await;
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(on_value(move |ValueCall { value, ops, .. }, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        }))
        .unwrap();
    drained().await;
    let frames = frames.lock().unwrap().clone();
    assert_eq!(frames.len(), 1);
    assert!(frames[0].1 == [Op::Replace(frames[0].0.clone())], "{:?}", ops_json(&frames[0].1));
    assert!(view_ids(&frames[0].0).len() <= 6);
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}

#[tokio::test]
async fn holds_every_entry_for_a_window_whose_bounds_are_the_largest_usize() {
    let (harness, root) = open().await;
    let written = notes(&root, 3, "note").await;
    let cases =
        [(window(usize::MAX, None), 0), (window(usize::MAX, Some(usize::MAX)), 0), (window(1, Some(usize::MAX)), 2)];
    for (bounds, first) in cases {
        assert_eq!(view_ids(&fresh_window(&root, bounds).await), ids(&written[first..]));
    }
    let resumed = ConversationWindow { from: Some(written[0].id), ..window(usize::MAX, Some(usize::MAX)) };
    assert_eq!(view_ids(&fresh_window(&root, resumed).await), ids(&written));
    harness.close(&context()).await.unwrap();
}
