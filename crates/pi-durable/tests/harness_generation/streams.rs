//! Streaming, deferred responses, and run cleanup cases of upstream `test/harness-generation.test.ts`.

use std::sync::{Arc, Mutex};

use pi_ai::{DeferredHandle, Message, StopReason, UserContent};
use pi_chord::Json;
use pi_durable::harness::live::{LIVE_DOC, LiveRun};
use pi_durable::harness::*;
use pi_durable::models::DeferredOption;
use pi_durable::session::Tx;
use pi_durable::storage::MemoryStorage;
use pi_durable::tasks::TaskOptions;
use pi_durable::testing::faux::{
    FauxDeferredOptions, FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message,
};
use pi_durable::*;
use serde_json::json;

use super::support::harness_support::*;
use super::support::models_support::{WrappedModels, scripted_stream};
use super::support::{ControlledStorage, change_record, change_value, content_kind, document_changes};
use super::{answer, empty, live};

/// Every published `pi.live` value of `harness`, in order.
fn live_publications(harness: &Harness) -> Arc<Mutex<Vec<Json>>> {
    let values = Arc::new(Mutex::new(Vec::new()));
    let sink = values.clone();
    let listener = move |publication: &CommitPublication, _: &pi_chord::Context| {
        for change in document_changes(publication) {
            if change_record(&change).kind == "pi.live"
                && let Some(value) = change_value(&change).filter(|value| !value.is_null())
            {
                sink.lock().unwrap().push(value);
            }
        }
        Ok(())
    };
    std::mem::forget(harness.subscribe_commits(listener).unwrap());
    values
}

fn partial_text(value: &Json) -> Option<String> {
    let message: Message = value.get("generation")?.get("message")?.deserialize_into().ok()?;
    text_of(Some(&message))
}

fn deferred_setup(pending_fetches: u32, poll_after_ms: u64) -> ChatSetup {
    let deferred = FauxDeferredOptions { pending_fetches, poll_after_ms: Some(poll_after_ms) };
    let setup = chat_setup(RegisterFauxProviderOptions { deferred: Some(deferred), ..Default::default() });
    let stream = ConversationStreamOptions { deferred: Some(DeferredOption::Enabled(true)), ..Default::default() };
    setup.settings.lock().unwrap().stream = Some(stream);
    setup
}

async fn deferred_poll(harness: &Harness, root: &Conversation) -> bool {
    live(harness, root).await.is_some_and(|value| !value["generation"]["deferred"].is_null())
}

#[tokio::test]
async fn stores_partials_as_deltas_and_a_complete_base_once_nothing_is_in_flight() {
    let token_size = pi_durable::testing::faux::FauxTokenSize { min: Some(1), max: Some(1) };
    let options = RegisterFauxProviderOptions {
        tokens_per_second: Some(200.0),
        token_size: Some(token_size),
        ..Default::default()
    };
    let setup = chat_setup(options);
    setup.faux.set_responses([answer(&"w".repeat(200))]);
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup).await;
    let scope = DocumentScope::Conversation { conversation_id: root.id };
    let address = DocumentAddress { kind: "pi.live".into(), scope, key: None };
    let record = storage.find_document(&address, DocumentPoint::Current, &ctx()).await.unwrap().unwrap();
    harness.resume().unwrap();
    root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let contents: Vec<&str> = storage
        .commits()
        .iter()
        .flatten()
        .filter(|write| matches!(write, StorageWrite::DocumentChange { id, .. } if *id == record.id))
        .filter_map(content_kind)
        .collect();
    // Streaming writes deltas; the commit that settles the answer clears generation and writes a base.
    assert!(contents.contains(&"delta"), "{contents:?}");
    assert_eq!(contents.last(), Some(&"base"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn polls_a_deferred_response_until_it_is_ready() {
    let setup = deferred_setup(1, 1);
    setup.faux.set_responses([answer("deferred answer")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let values = live_publications(&harness);
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    assert_eq!(setup.faux.state().deferred_fetch_count, 2);
    let values = values.lock().unwrap().clone();
    let polls: Vec<f64> =
        values.iter().filter_map(|value| value["generation"]["deferred"]["pollAt"].as_f64()).collect();
    assert_eq!(polls.len(), 2);
    assert!(polls[1] > polls[0]);
    let entries = all_entries(&root).await;
    let answer = entries.iter().find(|entry| Some(entry.id) == settled.answer).unwrap();
    assert_eq!(text_of(answer.model.as_ref().and_then(|model| model.first())).as_deref(), Some("deferred answer"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn cancels_a_deferred_response_when_aborted_during_polling() {
    let setup = deferred_setup(100, 60_000);
    setup.faux.set_responses([answer("never")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let task_id = run_task(&harness, &root).await;
    wait_for(|| deferred_poll(&harness, &root)).await;
    harness.abort_task(task_id, &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(setup.faux.state().cancelled_deferred.len(), 1);
    assert_eq!(live(&harness, &root).await, empty());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn reports_a_failed_deferred_cancellation_and_still_ends_the_run_aborted() {
    let base = deferred_setup(100, 60_000);
    base.faux.set_responses([answer("never")]);
    let setup = ChatSetup { models: WrappedModels::failing_cancel(base.models.clone(), "cancel failed"), ..base };
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let task_id = run_task(&harness, &root).await;
    wait_for(|| deferred_poll(&harness, &root)).await;
    harness.abort_task(task_id, &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert!(setup.reports().contains(&"cancel failed".to_owned()), "{:?}", setup.reports());
    assert_eq!(live(&harness, &root).await, empty());
    harness.close(&ctx()).await.unwrap();
}

fn pending(text: &str) -> pi_ai::AssistantMessage {
    faux_assistant_message(text, FauxMessageOptions::stop_reason(StopReason::Pending))
}

/// Whether a run over a stream that holds its partial for 300 ms publishes that partial under `progress`.
async fn published_partial(progress: Option<ProgressOverrides>) -> bool {
    let base = chat_setup(Default::default());
    base.settings.lock().unwrap().progress = progress;
    let stream = Arc::new(|_: &pi_ai::Model| scripted_stream(pending("partial"), 300, answer("final")));
    let setup = ChatSetup { models: WrappedModels::stream(base.models.clone(), stream), ..base };
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let values = live_publications(&harness);
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
    let values = values.lock().unwrap().clone();
    values.iter().any(|value| partial_text(value).as_deref() == Some("partial"))
}

#[tokio::test]
async fn commits_partials_no_more_often_than_progress_partial_interval_ms() {
    assert!(published_partial(None).await);
    let slow = ProgressOverrides { partial_interval_ms: Some(5000), ..Default::default() };
    assert!(!published_partial(Some(slow)).await);
}

#[tokio::test]
async fn commits_no_partial_for_a_response_that_turns_deferred_after_an_empty_start_event() {
    let base = chat_setup(Default::default());
    let stream = Arc::new(|_: &pi_ai::Model| {
        let handle = DeferredHandle {
            provider: "faux".into(),
            model_id: "faux-1".into(),
            api: "faux".into(),
            id: "handle-1".into(),
            expires_at: None,
            poll_after_ms: Some(60_000),
            data: None,
        };
        let options =
            FauxMessageOptions { deferred: Some(handle), ..FauxMessageOptions::stop_reason(StopReason::Deferred) };
        // Longer than the partial throttle: an empty partial would be committed here.
        let empty_start = faux_assistant_message(Vec::new(), FauxMessageOptions::stop_reason(StopReason::Pending));
        scripted_stream(empty_start, 300, faux_assistant_message(Vec::new(), options))
    });
    let setup = ChatSetup { models: WrappedModels::stream(base.models.clone(), stream), ..base };
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let values = live_publications(&harness);
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    harness.resume().unwrap();
    wait_for(|| deferred_poll(&harness, &root)).await;
    assert!(values.lock().unwrap().iter().all(|value| value["generation"]["message"].is_null()));
    harness.abort_task(run_task(&harness, &root).await, &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user"]);
    harness.close(&ctx()).await.unwrap();
}

// Not ported: "faults a run task, settling its inputs and converting the committed partial". Its stream ends with a
// final message that is not strict JSON; a Rust message is always JSON.

#[tokio::test]
async fn orphans_a_blocked_run_task_with_full_run_cleanup() {
    let setup = chat_setup(Default::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    // A run whose task was stored by a newer generation definition this process cannot run.
    let newer = define_task(
        TaskDefinition::new("pi.generation", 2, |_| Ok(Json::from(json!({ "phase": "run" }))))
            .phase("run", |_, _, _| async { Ok(()) })
            .abort(|_, _, _| async { Ok(()) }),
    );
    let id = root.id;
    let stage = move |tx: Tx| async move {
        let user = pi_ai::UserMessage { content: UserContent::Text("hi".into()), timestamp: 1 };
        let draft = EntryDraft { model: Some(vec![Message::User(user)]), ..EntryDraft::new("pi.user") };
        let entry = tx.append_entry(id, draft).await?;
        let create = SubmissionCreate {
            status: SubmissionStatus::Placed,
            entry: Some(entry.id),
            ..SubmissionCreate::queued(id, SubmissionType::Input, None)
        };
        let submission = tx.create_submission(create).await?;
        let task_id = tx.create_task(newer, Json::Null, TaskOptions::conversation(id)).await?;
        let run = LiveRun { task_id, inputs: vec![submission.id] };
        tx.doc(&LIVE_DOC, id).await?.edit(|live| live.run = Some(run))?;
        Ok((task_id, submission.id))
    };
    let (task_id, submission_id) = harness.commit(stage, &ctx()).await.unwrap();
    harness.resume().unwrap();
    let busy = SubmissionDraft::Input {
        request_id: None,
        content: UserContent::Text("busy".into()),
        when_busy: Some(WhenBusy::Reject),
        entry: None,
    };
    let error = root.submit(busy, &ctx()).await.err().unwrap();
    assert!(error.to_string().contains("is busy"), "{error}");
    assert_eq!(harness.abort_task(task_id, &ctx()).await.unwrap(), AbortTaskResult::Marked);
    let settled = harness.wait_for_task(task_id, &ctx()).await.unwrap();
    let outcome = serde_json::to_value(settled.state.outcome()).unwrap();
    assert_eq!(outcome, json!({ "status": "orphaned", "reason": "task_too_old" }));
    let record = harness.submission(submission_id, &ctx()).await.unwrap().unwrap().status(&ctx()).await.unwrap();
    assert_eq!((record.status, record.reason.as_deref()), (SubmissionStatus::Unanswered, Some("task_too_old")));
    assert_eq!(live(&harness, &root).await, empty());
    harness.close(&ctx()).await.unwrap();
}
