//! Port of upstream `test/harness-generation-recovery.test.ts`.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, Message, StopReason};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::models::{DeferredOption, create_models};
use pi_durable::session::Tx;
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::testing::faux::{
    FauxDeferredOptions, FauxMessageOptions, FauxResponseStep, FauxTokenSize, RegisterFauxProviderOptions,
    faux_assistant_message,
};
use pi_durable::{EntryRecord, SubmissionId, SubmissionRecord, SubmissionStatus, TaskId};
use serde_json::{Value, json};
use support::harness_support::*;
use support::task_support::aborted;

fn sqlite_path(directory: &tempfile::TempDir) -> String {
    directory.path().join("session.sqlite").to_string_lossy().into_owned()
}

async fn open(path: &str, setup: &ChatSetup) -> (Harness, Conversation) {
    open_chat(Arc::new(open_sqlite_storage(path).await.unwrap()), setup).await
}

fn message(text: &str) -> AssistantMessage {
    faux_assistant_message(text, FauxMessageOptions::default())
}

async fn live(harness: &Harness, root: &Conversation) -> Value {
    Value::from(&harness.snapshot(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap())
}

async fn checkpoint(harness: &Harness, id: TaskId) -> Option<Value> {
    let record = harness.get_task(id, &ctx()).await.unwrap()?;
    record.state.checkpoint().map(Value::from)
}

async fn settle(harness: &Harness, id: SubmissionId) -> SubmissionRecord {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap()
}

fn entry_kinds(entries: &[EntryRecord]) -> Vec<String> {
    kinds(entries)
}

fn roles(request: &pi_ai::Context) -> Vec<String> {
    request.messages.iter().map(|message| message.role().to_owned()).collect()
}

async fn submit(root: &Conversation, text: &str) -> SubmissionId {
    root.submit(SubmissionDraft::input(text), &ctx()).await.unwrap().id
}

#[tokio::test]
async fn reruns_preparation_interrupted_before_its_commit() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    let reached = support::Deferred::default();
    let block = Arc::new(AtomicBool::new(true));
    let (signal_reached, blocking) = (reached.clone(), block.clone());
    let preamble = section(
        "preamble",
        move |_, context: pi_chord::Context| {
            let (reached, blocking) = (signal_reached.clone(), blocking.swap(false, Ordering::SeqCst));
            async move {
                if blocking {
                    reached.resolve();
                    aborted(context.abort_signal().unwrap()).await;
                }
                Ok(Some("p".to_owned()))
            }
        },
        None,
    );
    setup.registry.install(Extension::new("section:preamble").section(preamble)).unwrap();
    setup.faux.set_responses([message("answer")]);
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    let id = submit(&root, "hi").await;
    reached.wait().await;
    let task_id = run_task(&harness, &root).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    assert_eq!(checkpoint(&harness, task_id).await, Some(json!({ "phase": "prepare", "attempt": 1 })));
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await.status, SubmissionStatus::Done);
    assert_eq!(entry_kinds(&all_entries(&root).await), ["pi.user", "pi.system", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn resends_a_request_interrupted_before_any_partial_without_repeating_preparation() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    add_section(&setup.registry, "preamble", "p", Some(false));
    let reached = support::Deferred::default();
    let sent = Arc::new(Mutex::new(Vec::new()));
    let timeouts = Arc::new(Mutex::new(Vec::new()));
    let signal_reached = reached.clone();
    let hold = FauxResponseStep::async_factory(move |_, options, _, _| {
        let (reached, signal) = (signal_reached.clone(), options.signal.clone().unwrap());
        Box::pin(async move {
            reached.resolve();
            Err(signal.cancelled().await.to_string())
        })
    });
    let (sent_sink, timeout_sink) = (sent.clone(), timeouts.clone());
    let record = FauxResponseStep::factory(move |request, options, _, _| {
        sent_sink.lock().unwrap().push(roles(request));
        timeout_sink.lock().unwrap().push(options.timeout_ms);
        message("answer")
    });
    setup.faux.set_responses([hold, record]);
    let (harness, root) = open(&path, &setup).await;
    setup.settings.lock().unwrap().stream =
        Some(ConversationStreamOptions { timeout_ms: Some(1234), ..Default::default() });
    harness.resume().unwrap();
    let id = submit(&root, "hi").await;
    reached.wait().await;
    let task_id = run_task(&harness, &root).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    let pinned = checkpoint(&harness, task_id).await.unwrap();
    assert_eq!((pinned["phase"].clone(), pinned["attempt"].clone()), (json!("request"), json!(1)));
    assert_eq!(
        (pinned["thinkingLevel"].clone(), pinned["streamOptions"]["timeoutMs"].clone()),
        (json!("off"), json!(1234))
    );
    // The resend uses the pinned request, not options changed after preparation.
    setup.settings.lock().unwrap().stream =
        Some(ConversationStreamOptions { timeout_ms: Some(999), ..Default::default() });
    assert_eq!(live(&harness, &root).await["generation"]["attempt"], json!(1));
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await.status, SubmissionStatus::Done);
    assert_eq!(*sent.lock().unwrap(), [["system", "user"]]);
    assert_eq!(*timeouts.lock().unwrap(), [Some(1234)]);
    assert_eq!(entry_kinds(&all_entries(&root).await), ["pi.user", "pi.system", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

fn partial_text(live: &Value) -> Option<String> {
    live["generation"]["message"]["content"][0]["text"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn converts_a_committed_partial_into_an_aborted_entry_and_resends_the_same_messages() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    let slow =
        chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(20.0), token_size, ..Default::default() });
    slow.faux.set_responses([message(&"z".repeat(400))]);
    let (harness, root) = open(&path, &slow).await;
    harness.resume().unwrap();
    let watch = harness.watch_doc(&LIVE_DOC, root.id, &ctx()).await.unwrap().unwrap();
    let watched = Arc::new(Mutex::new(None::<String>));
    let sink = watched.clone();
    watch
        .start(move |value, _, _| {
            if let Some(text) = partial_text(&Value::from(&value)) {
                *sink.lock().unwrap() = Some(text);
            }
            async { Ok(()) }
        })
        .unwrap();
    let id = submit(&root, "hi").await;
    wait_for(|| {
        let seen = watched.lock().unwrap().is_some();
        async move { seen }
    })
    .await;
    harness.close(&ctx()).await.unwrap();

    let setup = chat_setup(Default::default());
    let sent = Arc::new(Mutex::new(Vec::new()));
    let sink = sent.clone();
    setup.faux.set_responses([FauxResponseStep::factory(move |request, _, _, _| {
        sink.lock().unwrap().push(roles(request));
        message("answer")
    })]);
    let (harness, root) = open(&path, &setup).await;
    let partial = partial_text(&live(&harness, &root).await).unwrap();
    // Everything observers saw before the crash is durable.
    assert!(partial.starts_with(watched.lock().unwrap().as_deref().unwrap()));
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await.status, SubmissionStatus::Done);
    assert_eq!(*sent.lock().unwrap(), [["user"]]);
    let entries = all_entries(&root).await;
    assert_eq!(entry_kinds(&entries), ["pi.user", "pi.assistant", "pi.assistant"]);
    let Some(Message::Assistant(converted)) = entries[1].model.as_ref().and_then(|model| model.first()) else {
        panic!("expected an assistant message");
    };
    assert_eq!(converted.stop_reason, StopReason::Aborted);
    assert_eq!(text_of(entries[1].model.as_ref().and_then(|model| model.first())), Some(partial));
    assert_eq!(live(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn resumes_a_retry_backoff_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    *setup.now.lock().unwrap() = Some(1_000.0);
    let options = FauxMessageOptions {
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::stop_reason(StopReason::Error)
    };
    setup.faux.set_responses([faux_assistant_message(Vec::new(), options), message("recovered")]);
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    setup.settings.lock().unwrap().retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(2),
        base_delay_ms: Some(60_000),
        ..Default::default()
    });
    let id = submit(&root, "hi").await;
    wait_for(|| async { live(&harness, &root).await["generation"].get("retry").is_some() }).await;
    let task_id = run_task(&harness, &root).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    assert_eq!(checkpoint(&harness, task_id).await, Some(json!({ "phase": "retry", "attempt": 1, "until": 61_000 })));
    let expected = json!({
        "run": { "taskId": task_id, "inputs": [id] },
        "generation": { "attempt": 1, "retry": { "at": 61_000, "error": "503 Service Unavailable" } },
    });
    assert_eq!(live(&harness, &root).await, expected);
    *setup.now.lock().unwrap() = Some(61_000.0);
    harness.resume().unwrap();
    assert_eq!(settle(&harness, id).await.status, SubmissionStatus::Done);
    assert_eq!(entry_kinds(&all_entries(&root).await), ["pi.user", "pi.assistant", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

fn deferred_setup() -> ChatSetup {
    let deferred = Some(FauxDeferredOptions { poll_after_ms: Some(60_000), ..Default::default() });
    chat_setup(RegisterFauxProviderOptions { deferred, ..Default::default() })
}

fn defer_streams(setup: &ChatSetup) {
    let stream = ConversationStreamOptions { deferred: Some(DeferredOption::Enabled(true)), ..Default::default() };
    setup.settings.lock().unwrap().stream = Some(stream);
}

async fn answer_text(root: &Conversation, settled: &SubmissionRecord) -> Option<String> {
    let answer = settled.answer.unwrap();
    let entry = root.commit(move |tx: Tx| async move { tx.entry(answer).await }, &ctx()).await.unwrap();
    text_of(entry.as_ref().and_then(|entry| entry.model.as_ref()?.first()))
}

#[tokio::test]
async fn resumes_polling_a_deferred_response_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = deferred_setup();
    *setup.now.lock().unwrap() = Some(1_000.0);
    setup.faux.set_responses([message("deferred answer")]);
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    defer_streams(&setup);
    let id = submit(&root, "hi").await;
    wait_for(|| async { live(&harness, &root).await["generation"].get("deferred").is_some() }).await;
    let task_id = run_task(&harness, &root).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    let pinned = checkpoint(&harness, task_id).await.unwrap();
    assert_eq!(
        (pinned["phase"].clone(), pinned["attempt"].clone(), pinned["pollAt"].clone()),
        (json!("poll"), json!(1), json!(61_000))
    );
    *setup.now.lock().unwrap() = Some(61_000.0);
    harness.resume().unwrap();
    let settled = settle(&harness, id).await;
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("deferred answer"));
    assert_eq!(setup.faux.state().deferred_fetch_count, 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_no_model_when_the_pinned_model_is_gone_after_reopen_in_request_and_in_poll() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = deferred_setup();
    let (busy, reached) = unanswered();
    setup.faux.set_responses([busy, message("deferred").into()]);
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    let requesting = submit(&root, "one").await;
    reached.notified().await;
    harness.close(&ctx()).await.unwrap();

    // Reopened without the faux provider: the request's pinned model is unknown.
    let empty = ChatSetup { models: Arc::new(create_models()), ..setup.clone() };
    let (harness, _) = open(&path, &empty).await;
    harness.resume().unwrap();
    let settled = settle(&harness, requesting).await;
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("no_model")));
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    defer_streams(&setup);
    harness.resume().unwrap();
    let polling = submit(&root, "two").await;
    wait_for(|| async { live(&harness, &root).await["generation"].get("deferred").is_some() }).await;
    harness.close(&ctx()).await.unwrap();

    let (harness, _) = open(&path, &empty).await;
    harness.resume().unwrap();
    let settled = settle(&harness, polling).await;
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("no_model")));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn runs_a_print_style_turn_and_reads_the_durable_answer_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = sqlite_path(&directory);
    let setup = chat_setup(Default::default());
    add_section(&setup.registry, "preamble", "You are terse.", Some(false));
    setup.faux.set_responses([message("42")]);
    let (harness, root) = open(&path, &setup).await;
    harness.resume().unwrap();
    let submission = root.submit(SubmissionDraft::input("answer?"), &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("42"));
    let answer = settled.answer.unwrap();
    let entry = root.commit(move |tx: Tx| async move { tx.entry(answer).await }, &ctx()).await.unwrap();
    harness.close(&ctx()).await.unwrap();

    let (harness, root) = open(&path, &setup).await;
    let status = harness.submission(submission.id, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
    assert_eq!(status, settled);
    let reread = root.commit(move |tx: Tx| async move { tx.entry(answer).await }, &ctx()).await.unwrap();
    assert_eq!(reread, entry);
    harness.close(&ctx()).await.unwrap();
}
