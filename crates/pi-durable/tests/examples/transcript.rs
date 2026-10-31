//! Example 31: a client's transcript over the windowed conversation view, with a stall, gaps, earlier pages, and a
//! resume after a drop.

use std::sync::Mutex;

use futures::FutureExt;

use super::*;

/// Commit `n` chat lines after `count`, one entry each.
async fn chat(root: &Conversation, count: &Mutex<u64>, n: usize) {
    for _ in 0..n {
        let line = {
            let mut count = count.lock().unwrap();
            *count += 1;
            *count
        };
        let (id, data) = (root.id, Json::from(format!("line {line}")));
        let draft = EntryDraft { data: Some(data), ..EntryDraft::new("chat") };
        root.commit(move |tx: Tx| async move { tx.append_entry(id, draft).await }, &context()).await.unwrap();
    }
}

/// One row per item: ranges of entries, gaps, and the "load earlier" marker.
fn render(view: &TranscriptView) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut run: Option<(u64, u64)> = None;
    let flush = |run: &mut Option<(u64, u64)>, rows: &mut Vec<String>| {
        if let Some((first, last)) = run.take() {
            rows.push(if first == last { first.to_string() } else { format!("{first}..{last}") });
        }
    };
    for item in view.items.iter() {
        let TranscriptItem::Entry(entry) = item else {
            flush(&mut run, &mut rows);
            rows.push(if matches!(item, TranscriptItem::Gap { .. }) { "[load more]" } else { "[load earlier]" }.into());
            continue;
        };
        let data = entry.data.as_ref().and_then(Json::as_str).unwrap_or_default();
        let line: u64 = data.trim_start_matches("line ").parse().unwrap();
        run = Some(run.map_or((line, line), |(first, _)| (first, line)));
    }
    flush(&mut run, &mut rows);
    rows.join(" ")
}

fn first_gap(view: &TranscriptView) -> Option<TranscriptItem> {
    view.items.iter().find(|item| matches!(item, TranscriptItem::Gap { .. })).cloned()
}

async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

/// Merge every value of `watch` into `transcript`, keeping the newest view in `latest`.
fn follow(
    watch: &ConversationWatch,
    transcript: &Arc<ConversationTranscript>,
    latest: &Arc<Mutex<Arc<TranscriptView>>>,
) {
    let (merging, sink) = (transcript.clone(), latest.clone());
    let listen = move |pi_chord::WatchChange { value, .. }, _| {
        let merged = merging.merge(&value).map(|view| *sink.lock().unwrap() = view);
        async move { merged }
    };
    watch.start(listen).unwrap();
}

#[tokio::test]
async fn ex31_transcript_merges_windows_fills_gaps_loads_earlier_and_resumes() {
    let (harness, root) = open_chat(memory(), &chat_setup(Default::default())).await;
    let count = Mutex::new(0);
    chat(&root, &count, 200).await;
    let pages = root.clone();
    let load: EntryLoader = Arc::new(move |range: EntryRange, limit: usize, context: Context| {
        let pages = pages.clone();
        async move { pages.entries(range, limit, None, &context).await }.boxed()
    });
    let transcript = Arc::new(ConversationTranscript::new(load, Some(20)).unwrap());
    let window = ConversationWindow { min: Some(10), max: Some(20), from: None };
    let mut lines = Vec::new();
    let watch = root.watch(&context(), Some(window)).await.unwrap();
    let latest = Arc::new(Mutex::new(transcript.merge(&watch.value()).unwrap()));
    lines.push(render(&latest.lock().unwrap()));
    follow(&watch, &transcript, &latest);
    chat(&root, &count, 15).await;
    settle().await;
    lines.push(render(&latest.lock().unwrap()));
    lines.push(render(&transcript.load_earlier(&context()).await.unwrap()));

    watch.stop().await;
    chat(&root, &count, 40).await;
    let resume =
        |transcript: &ConversationTranscript| ConversationWindow { from: transcript.resume().unwrap().from, ..window };
    let watch = root.watch(&context(), Some(resume(&transcript))).await.unwrap();
    let mut view = transcript.merge(&watch.value()).unwrap();
    lines.push(render(&view));
    while let Some(gap) = first_gap(&view) {
        view = transcript.fill_gap(&gap, &context()).await.unwrap();
        lines.push(render(&view));
    }
    assert_eq!(
        lines,
        [
            "[load earlier] 191..200",
            "[load earlier] 191..215",
            "[load earlier] 171..215",
            "[load earlier] 171..215 [load more] 246..255",
            "[load earlier] 171..235 [load more] 246..255",
            "[load earlier] 171..255",
        ]
    );

    // A short drop resumes without a gap: the window starts right after the newest entry.
    watch.stop().await;
    chat(&root, &count, 3).await;
    let watch = root.watch(&context(), Some(resume(&transcript))).await.unwrap();
    assert_eq!(watch.value().get("entries").and_then(Json::as_array).map(Vec::len), Some(3));
    assert_eq!(render(&transcript.merge(&watch.value()).unwrap()), "[load earlier] 171..258");
    watch.stop().await;
    harness.close(&context()).await.unwrap();
}
