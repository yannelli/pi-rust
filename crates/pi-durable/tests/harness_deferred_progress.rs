//! Port of upstream `test/harness-deferred-progress.test.ts`: generation partials and tool restart counts. A crash
//! fails the Harness, so nothing more reaches Storage. Other tool output, compaction, and third-party cases wait.

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::Message;
use pi_chord::Json;
use pi_chord::delta::apply_immutable;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxTokenSize, RegisterFauxProviderOptions, faux_assistant_message,
};
use pi_durable::*;
use support::harness_support::*;
use support::{ControlledStorage, observe_commits};

/// A slow stream: 4 characters 80 times a second.
fn slow_setup(persist_within_ms: f64) -> ChatSetup {
    let token_size = Some(FauxTokenSize { min: Some(4), max: Some(4) });
    let setup =
        chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(80.0), token_size, ..Default::default() });
    let progress = Some(ProgressOverrides { persist_within_ms: Some(persist_within_ms), ..Default::default() });
    let mut settings = setup.settings.lock().unwrap();
    settings.progress = progress;
    // A request a crash interrupted is a failed attempt: retry at once.
    settings.retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(5),
        base_delay_ms: Some(1),
        ..Default::default()
    });
    drop(settings);
    setup
}

fn long() -> String {
    "streamed ".repeat(400)
}

fn answer(text: &str) -> pi_ai::AssistantMessage {
    faux_assistant_message(text, FauxMessageOptions::default())
}

struct Opened {
    harness: Harness,
    root: Conversation,
    /// The partial text of every `pi.live` revision memory adopted, the last one last.
    memory: Arc<Mutex<Vec<String>>>,
}

/// Open (or reopen) a Harness over `storage` and let it run tasks.
async fn open(storage: &Arc<ControlledStorage>, setup: &ChatSetup) -> Opened {
    storage.reopen();
    let (harness, root) = open_chat(storage.clone(), setup).await;
    harness.resume().unwrap();
    let memory: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = memory.clone();
    let observing = observe_commits(&harness, move |publication, _| {
        for change in &publication.changes {
            let CommitChange::Document(DocumentCommitChange::Document { record, value: Some(value), .. }) = change
            else {
                continue;
            };
            if record.kind == "pi.live" && value["generation"].get("message").is_some() {
                sink.lock().unwrap().push(partial_text(value).unwrap_or_default());
            }
        }
        Ok(())
    });
    std::mem::forget(observing.unwrap());
    Opened { harness, root, memory }
}

fn last_memory(opened: &Opened) -> usize {
    opened.memory.lock().unwrap().last().map_or(0, String::len)
}

/// Fail the Harness now: nothing more is stored, held changes are lost with it.
async fn crash(harness: &Harness) {
    harness.fail(DurableError::error("crash"), None);
    harness.closed().await;
}

/// `pi.live` as Storage has it, read past the Session, also after a crash closed it.
async fn stored_live(storage: &ControlledStorage, conversation_id: ConversationId) -> Option<Json> {
    storage.reopen();
    let scope = DocumentScope::Conversation { conversation_id };
    let address = DocumentAddress { kind: "pi.live".into(), scope, key: None };
    let record = storage.find_document(&address, DocumentPoint::Current, &ctx()).await.unwrap()?;
    storage.document(record.id, DocumentPoint::Current, &ctx()).await.unwrap().map(|stored| stored.value)
}

fn partial_text(live: &Json) -> Option<String> {
    let content = live["generation"]["message"]["content"].as_array()?;
    let block = content.iter().find(|block| block["type"] == Json::from("text"))?;
    Some(block["text"].as_str().unwrap_or_default().to_owned())
}

/// Assistant entries: their text and stop reason.
fn answers(entries: &[EntryRecord]) -> Vec<(Option<String>, String)> {
    let assistants = entries.iter().filter(|entry| entry.kind == "pi.assistant");
    let answer = |entry: &EntryRecord| {
        let message = entry.model.as_ref().and_then(|model| model.first());
        let stop = match message {
            Some(Message::Assistant(assistant)) => format!("{:?}", assistant.stop_reason).to_lowercase(),
            _ => String::new(),
        };
        (text_of(message), stop)
    };
    assistants.map(answer).collect()
}

async fn stored_text(storage: &ControlledStorage, id: ConversationId) -> Option<String> {
    stored_live(storage, id).await.as_ref().and_then(partial_text)
}

#[tokio::test]
async fn hold_partials_in_memory_and_store_them_together_at_most_every_persist_within_ms() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = slow_setup(400.0);
    setup.faux.set_responses([answer(&long()[..640])]);
    let opened = open(&storage, &setup).await;
    let from = storage.commit_count();
    let submission = opened.root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap();
    let writes_live = storage.commits()[from..]
        .iter()
        .filter(|batch| batch.iter().any(|write| matches!(write, StorageWrite::DocumentChange { .. })))
        .count();
    // About 2 s of streaming at 100 ms per partial: some 20 partials in memory, a store about every 400 ms.
    let memory = opened.memory.lock().unwrap().len();
    assert!(memory > 12, "{memory} partials");
    assert!((writes_live as f64) < memory as f64 / 1.5, "{writes_live} stores for {memory} partials");
    opened.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn converts_what_was_stored_into_the_aborted_entry_after_a_crash_and_retries_the_charged_attempt() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = slow_setup(150.0);
    setup.faux.set_responses([answer(&long())]);
    let opened = open(&storage, &setup).await;
    // A replica fed by subscribe_commits() alone, as a replication feed would be.
    let replica = Arc::new(Mutex::new(stored_live(&storage, opened.root.id).await));
    let sink = replica.clone();
    let feed = opened.harness.subscribe_commits(move |publication, _| {
        for change in &publication.changes {
            if let CommitChange::Document(DocumentCommitChange::Document { record, ops, .. }) = change
                && record.kind == "pi.live"
            {
                let mut replica = sink.lock().unwrap();
                *replica = Some(apply_immutable(replica.as_ref(), ops).unwrap());
            }
        }
        Ok(())
    });
    std::mem::forget(feed.unwrap());
    let submission = opened.root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let id = opened.root.id;
    let storage_ref = storage.clone();
    wait_for(|| {
        let storage = storage_ref.clone();
        async move { stored_text(&storage, id).await.map_or(0, |text| text.len()) > 40 }
    })
    .await;
    let in_memory = opened.memory.lock().unwrap().last().cloned().unwrap();
    crash(&opened.harness).await;
    let stored = stored_text(&storage, id).await.unwrap();
    // What Storage has is a state memory had, behind it by at most the window, and what the replica has.
    assert!(opened.memory.lock().unwrap().contains(&stored));
    assert!(in_memory.starts_with(&stored));
    let live = stored_live(&storage, id).await;
    assert_eq!(*replica.lock().unwrap(), live);

    setup.faux.set_responses([answer("answer")]);
    let opened = open(&storage, &setup).await;
    let again = opened.harness.submission(submission.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(again.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let entries = all_entries(&opened.root).await;
    let expected = [(Some(stored), "aborted".to_owned()), (Some("answer".to_owned()), "stop".to_owned())];
    assert_eq!(answers(&entries), expected);
    let live = opened.harness.snapshot(&LIVE_DOC, opened.root.id, &ctx()).await.unwrap();
    assert_eq!(live, Some(Json::object(Default::default())));
    opened.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn leaves_no_aborted_entry_for_a_stream_that_crashed_before_its_first_store() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = slow_setup(f64::INFINITY);
    setup.faux.set_responses([answer(&long())]);
    let opened = open(&storage, &setup).await;
    let submission = opened.root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    wait_for(|| {
        let length = last_memory(&opened);
        async move { length > 40 }
    })
    .await;
    crash(&opened.harness).await;
    let live = stored_live(&storage, opened.root.id).await.unwrap();
    assert_eq!(live["generation"]["attempt"], Json::from(1));
    assert_eq!(partial_text(&live), None);

    setup.faux.set_responses([answer("answer")]);
    let opened = open(&storage, &setup).await;
    let again = opened.harness.submission(submission.id, &ctx()).await.unwrap().unwrap();
    assert_eq!(again.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(answers(&all_entries(&opened.root).await), [(Some("answer".to_owned()), "stop".to_owned())]);
    opened.harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn stores_everything_held_at_a_clean_close() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = slow_setup(f64::INFINITY);
    setup.faux.set_responses([answer(&long())]);
    let opened = open(&storage, &setup).await;
    let submission = opened.root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    wait_for(|| {
        let length = last_memory(&opened);
        async move { length > 40 }
    })
    .await;
    opened.harness.close(&ctx()).await.unwrap();
    let at_close = opened.memory.lock().unwrap().last().cloned().unwrap();
    assert_eq!(stored_text(&storage, opened.root.id).await, Some(at_close.clone()));

    setup.faux.set_responses([answer("answer")]);
    let opened = open(&storage, &setup).await;
    let again = opened.harness.submission(submission.id, &ctx()).await.unwrap().unwrap();
    again.wait(&ctx()).await.unwrap();
    assert_eq!(answers(&all_entries(&opened.root).await)[0], (Some(at_close), "aborted".to_owned()));
    opened.harness.close(&ctx()).await.unwrap();
}

/// The first tool slot's output in `pi.live` as Storage has it.
async fn stored_output(storage: &ControlledStorage, id: ConversationId) -> Option<String> {
    let live = stored_live(storage, id).await?;
    live["tools"].as_array()?.first()?["output"].as_str().map(str::to_owned)
}

// A generation's request is charged at once after a crash, so its partials never stream with a count; a replay-safe
// tool reruns and streams its output with one.
#[tokio::test]
async fn never_counts_progress_commits_as_progress_of_a_task_cut_off_again_and_again() {
    let storage = Arc::new(ControlledStorage::default());
    let setup = slow_setup(50.0);
    let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = runs.clone();
    let work = define_tool("work", "work", support::chat::no_parameters(), move |_, api, context| {
        let run = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        async move {
            let signal = context.abort_signal().unwrap();
            for line in 0.. {
                if signal.aborted() {
                    break;
                }
                api.output(&format!("run {run} line {line}\n"))?;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Ok(ToolExecutionResult::default())
        }
    })
    .with_replay(ToolReplay::Safe);
    support::chat::add_tool(&setup, work);
    setup.faux.set_responses([support::chat::call("work", "c1")]);
    let mut opened = open(&storage, &setup).await;
    opened.root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    for crashes in 1..=2u32 {
        // This run stored output of its own, a progress commit, before it was cut off.
        let (storage_ref, id, prefix) = (storage.clone(), opened.root.id, format!("run {crashes} "));
        wait_for(|| {
            let (storage, prefix) = (storage_ref.clone(), prefix.clone());
            async move { stored_output(&storage, id).await.is_some_and(|output| output.starts_with(&prefix)) }
        })
        .await;
        crash(&opened.harness).await;
        opened = open(&storage, &setup).await;
        let query = TaskQuery { kind: Some("pi.tool".into()), ..TaskQuery::default() };
        let scan = move |tx: pi_durable::session::Tx| async move { tx.scan_tasks(query, 1, None).await };
        let tool = opened.harness.commit(scan, &ctx()).await.unwrap().items.remove(0);
        assert_eq!(tool.restarts, Some(crashes));
    }
    opened.harness.close(&ctx()).await.unwrap();
}
