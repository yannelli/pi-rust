//! Port of upstream `test/harness-restart-failure.test.ts`: a generation's request a crash interrupted is a failed
//! attempt under the retry policy, and other work ends visibly once cut off `restart_limit` times in a row.

mod support;

#[path = "harness_restart_failure/deferred.rs"]
mod deferred;
#[path = "harness_restart_failure/generation.rs"]
mod generation;
#[path = "harness_restart_failure/limits.rs"]
mod limits;
#[path = "harness_restart_failure/others.rs"]
mod others;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::Message;
use pi_chord::Json;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, FauxTokenSize, RegisterFauxProviderOptions, faux_assistant_message,
};
use pi_durable::*;
use serde_json::{Value, json};
use support::harness_support::*;
use support::{ControlledStorage, observe_commits};

/// Open (or reopen after a crash) a chat over `storage`.
async fn open(storage: &Arc<ControlledStorage>, setup: &ChatSetup) -> (Harness, Conversation) {
    storage.reopen();
    open_chat(storage.clone(), setup).await
}

/// A crash: from now on nothing reaches Storage, not even the records a close writes for the tasks it stops.
async fn kill(harness: &Harness) {
    harness.fail(DurableError::error("crash"), None);
    let closed = tokio::time::timeout(Duration::from_secs(10), harness.closed()).await;
    closed.expect("a crashed Harness kept running: an invocation ignores its abort signal");
}

async fn live(harness: &Harness) -> Value {
    let value = harness.snapshot(&LIVE_DOC, ROOT_CONVERSATION_ID, &ctx()).await.unwrap();
    value.map(|value| value.to_serde()).unwrap_or_else(|| json!({}))
}

/// Every `pi.live` value memory adopts while this lives; dropping it detaches the observer.
struct LiveValues {
    values: Arc<Mutex<Vec<Value>>>,
    observing: pi_chord::Unsubscribe,
}

impl LiveValues {
    fn get(&self) -> Vec<Value> {
        self.values.lock().unwrap().clone()
    }
}

impl Drop for LiveValues {
    fn drop(&mut self) {
        self.observing.unsubscribe();
    }
}

fn live_values(harness: &Harness) -> LiveValues {
    let values: Arc<Mutex<Vec<Value>>> = Arc::default();
    let sink = values.clone();
    let observing = observe_commits(harness, move |publication, _| {
        for change in &publication.changes {
            if let CommitChange::Document(DocumentCommitChange::Document { record, value: Some(value), .. }) = change
                && record.kind == "pi.live"
            {
                sink.lock().unwrap().push(value.to_serde());
            }
        }
        Ok(())
    });
    LiveValues { values, observing: observing.unwrap() }
}

/// Let deferred progress commits (`persist_within_ms: 0`) reach Storage before a crash.
async fn stored() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn retry(setup: &ChatSetup, enabled: bool, max_retries: u32, base_delay_ms: u64) {
    let overrides = RetryOverrides {
        enabled: Some(enabled),
        max_retries: Some(max_retries),
        base_delay_ms: Some(base_delay_ms),
        ..RetryOverrides::default()
    };
    setup.settings.lock().unwrap().retry = Some(overrides);
}

fn restart_limit(setup: &ChatSetup, limit: f64) {
    setup.settings.lock().unwrap().restart_limit = Some(limit);
}

fn answer(text: &str) -> FauxResponseStep {
    faux_assistant_message(text, FauxMessageOptions::default()).into()
}

/// A model that streams a long answer slowly; partials are stored as soon as they are committed.
fn streaming() -> ChatSetup {
    let token_size = Some(FauxTokenSize { min: Some(1), max: Some(1) });
    let setup =
        chat_setup(RegisterFauxProviderOptions { tokens_per_second: Some(20.0), token_size, ..Default::default() });
    setup.faux.set_responses([answer(&"z".repeat(400))]);
    let progress = Some(ProgressOverrides { persist_within_ms: Some(0.0), ..Default::default() });
    setup.settings.lock().unwrap().progress = progress;
    setup
}

fn partial_text(live: &Value) -> Option<String> {
    let content = live["generation"]["message"]["content"].as_array()?;
    content.iter().find(|block| block["type"] == "text")?["text"].as_str().map(str::to_owned)
}

async fn run_task_id(harness: &Harness) -> TaskId {
    TaskId(live(harness).await["run"]["taskId"].as_u64().unwrap())
}

/// Submit an input to a fresh chat and crash once its streamed partial is stored.
async fn crash_while_streaming(storage: &Arc<ControlledStorage>, setup: &ChatSetup) -> (SubmissionId, TaskId) {
    let (harness, root) = open(storage, setup).await;
    harness.resume().unwrap();
    let id = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().id;
    wait_for(|| async { live(&harness).await["generation"].get("message").is_some() }).await;
    let task_id = run_task_id(&harness).await;
    stored().await;
    kill(&harness).await;
    (id, task_id)
}

/// Reopen after a crash in the middle of a request, with another streaming answer, and crash again.
async fn crash_again(storage: &Arc<ControlledStorage>, setup: &ChatSetup, task_id: TaskId) {
    let (harness, _) = open(storage, setup).await;
    harness.resume().unwrap();
    // The first attempt was charged; the second one streams.
    wait_for(|| async {
        let record = harness.get_task(task_id, &ctx()).await.unwrap();
        let checkpoint = match record.map(|record| record.state) {
            Some(TaskState::Running { checkpoint }) => checkpoint.to_serde(),
            _ => return false,
        };
        checkpoint["phase"] == "request" && checkpoint["attempt"] == 2
    })
    .await;
    wait_for(|| async { live(&harness).await["generation"].get("message").is_some() }).await;
    stored().await;
    kill(&harness).await;
}

async fn settled(harness: &Harness, id: SubmissionId) -> SubmissionRecord {
    harness.submission(id, &ctx()).await.unwrap().unwrap().wait(&ctx()).await.unwrap()
}

fn detail(record: &SubmissionRecord) -> Option<Value> {
    record.detail.as_ref().map(Json::to_serde)
}

fn stop_reasons(entries: &[EntryRecord]) -> Vec<String> {
    let reason = |entry: &EntryRecord| match entry.model.as_ref().and_then(|model| model.first()) {
        Some(Message::Assistant(message)) => format!("{:?}", message.stop_reason).to_lowercase(),
        _ => String::new(),
    };
    entries.iter().map(reason).collect()
}

const STOPPED_ANSWERING: &str = "The host stopped while the model was answering, on attempt";

#[tokio::test]
async fn charges_the_attempt_keeps_the_partial_waits_for_the_retry_backoff_and_answers_on_the_next_attempt() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, true, 2, 1);
    let (id, task_id) = crash_while_streaming(&storage, &first).await;

    let setup = chat_setup(Default::default());
    retry(&setup, true, 2, 1);
    let sent: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let log = sent.clone();
    setup.faux.set_responses([FauxResponseStep::factory(move |request, _, _, _| {
        let roles = request.messages.iter().map(|message| message.role().to_owned()).collect();
        log.lock().unwrap().push(roles);
        faux_assistant_message("answer", FauxMessageOptions::default())
    })]);
    let (harness, root) = open(&storage, &setup).await;
    let values = live_values(&harness);
    let partial = partial_text(&live(&harness).await).unwrap();
    assert_eq!(harness.get_task(task_id, &ctx()).await.unwrap().unwrap().restarts, Some(1));
    harness.resume().unwrap();
    assert_eq!(settled(&harness, id).await.status, SubmissionStatus::Done);
    // The backoff showed in `pi.live` with why.
    let values = values.get();
    let retry = values.iter().find(|value| value["generation"].get("retry").is_some()).unwrap()["generation"].clone();
    assert_eq!(retry["attempt"], 1);
    assert!(retry["retry"]["at"].is_number(), "{retry}");
    assert_eq!(retry["retry"]["error"], format!("{STOPPED_ANSWERING} 1"));
    assert_eq!(retry["retry"].as_object().unwrap().len(), 2, "{retry}");
    assert_eq!(retry.as_object().unwrap().len(), 2, "{retry}");
    assert!(values.iter().any(|value| value["generation"]["attempt"] == 2));
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.assistant", "pi.assistant"]);
    assert_eq!(stop_reasons(&entries[1..2]), ["aborted"]);
    assert_eq!(text_of(entries[1].model.as_ref().and_then(|model| model.first())), Some(partial));
    // The aborted partial is excluded from the next request.
    assert_eq!(*sent.lock().unwrap(), [vec!["user".to_owned()]]);
    assert_eq!(live(&harness).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn sleeps_the_backoff_of_settings_retry_before_the_next_attempt() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, true, 2, 60_000);
    let (_, task_id) = crash_while_streaming(&storage, &first).await;

    let setup = chat_setup(Default::default());
    retry(&setup, true, 2, 60_000);
    let now = 1_000_000.0;
    *setup.now.lock().unwrap() = Some(now);
    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let status = || async { harness.get_task(task_id, &ctx()).await.unwrap().unwrap().status() };
    wait_for(|| async { status().await == TaskStatus::Sleeping }).await;
    let record = harness.get_task(task_id, &ctx()).await.unwrap().unwrap();
    let state = serde_json::to_value(&record.state).unwrap();
    assert_eq!((&state["status"], &state["until"]), (&json!("sleeping"), &json!(now + 60_000.0)));
    assert_eq!(state["checkpoint"]["phase"], "retry");
    assert_eq!(record.restarts, None);
    let error = format!("{STOPPED_ANSWERING} 1");
    let generation = json!({ "attempt": 1, "retry": { "at": now + 60_000.0, "error": error } });
    assert_eq!(Json::from(live(&harness).await["generation"].clone()), Json::from(generation));
    assert_eq!(harness.wake().unwrap(), Wake { active: false, at: Some(now + 60_000.0) });
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_run_interrupted_once_no_retry_is_left_keeping_every_partial() {
    let storage = Arc::new(ControlledStorage::default());
    let first = streaming();
    retry(&first, true, 1, 1);
    let (id, task_id) = crash_while_streaming(&storage, &first).await;
    // Attempt 2 is interrupted too.
    let second = streaming();
    retry(&second, true, 1, 1);
    crash_again(&storage, &second, task_id).await;

    let setup = chat_setup(Default::default());
    retry(&setup, true, 1, 1);
    let (harness, root) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let text = format!("{STOPPED_ANSWERING} 2");
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("interrupted")));
    assert_eq!(detail(&record), Some(json!(text)));
    let failure = json!({ "taskId": task_id, "inputs": [id], "reason": "interrupted", "detail": text });
    assert_eq!(live(&harness).await, json!({ "runFailure": failure }));
    let outcome = harness.wait_for_task(task_id, &ctx()).await.unwrap().state;
    let outcome = serde_json::to_value(outcome.outcome()).unwrap();
    assert_eq!(
        outcome,
        json!({ "status": "failed", "error": { "message": text, "detail": { "reason": "interrupted" } } })
    );
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.assistant", "pi.assistant"]);
    assert_eq!(stop_reasons(&entries[1..]), ["aborted", "aborted"]);
    // No request was sent after the second crash.
    assert_eq!(setup.faux.state().call_count, 0);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_the_run_at_once_when_retries_are_disabled() {
    let storage = Arc::new(ControlledStorage::default());
    let (id, _) = crash_while_streaming(&storage, &streaming()).await;
    let setup = chat_setup(Default::default());
    setup.settings.lock().unwrap().retry = Some(RetryOverrides { enabled: Some(false), ..RetryOverrides::default() });
    let (harness, _) = open(&storage, &setup).await;
    harness.resume().unwrap();
    let record = settled(&harness, id).await;
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("interrupted")));
    harness.close(&ctx()).await.unwrap();
}
