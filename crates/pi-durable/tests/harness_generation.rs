//! Port of upstream `test/harness-generation.test.ts`. This file holds the core cases; `harness_generation/` holds
//! the streaming and deferred cases (`streams.rs`) and the request option and settings cases (`options.rs`).

#[path = "harness_generation/options.rs"]
mod options;
#[path = "harness_generation/streams.rs"]
mod streams;
mod support;

use std::sync::Arc;

use pi_ai::{Message, StopReason};
use pi_chord::Json;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{FauxMessageOptions, RegisterFauxProviderOptions, faux_assistant_message};
use pi_durable::{SubmissionId, SubmissionStatus, TaskId, TaskOutcome};
use serde_json::json;
use support::harness_support::*;

pub fn error_503() -> pi_ai::AssistantMessage {
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("503 Service Unavailable".into()),
        ..FauxMessageOptions::default()
    };
    faux_assistant_message(Vec::new(), options)
}

pub fn answer(text: &str) -> pi_ai::AssistantMessage {
    faux_assistant_message(text, FauxMessageOptions::default())
}

pub async fn live(harness: &Harness, conversation: &Conversation) -> Option<Json> {
    harness.snapshot(&LIVE_DOC, conversation.id, &ctx()).await.unwrap()
}

pub fn empty() -> Option<Json> {
    Some(Json::from(json!({})))
}

/// The `pi.live` left by run `task_id` that ended without an answer to `input`.
pub fn failed(task_id: TaskId, input: SubmissionId, reason: &str) -> Option<Json> {
    Some(Json::from(json!({ "runFailure": { "taskId": task_id, "inputs": [input], "reason": reason } })))
}

#[tokio::test]
async fn answers_an_input_and_settles_its_submission() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_section(&setup.registry, "preamble", "You are helpful.", Some(false));
    setup.faux.set_responses([answer("Hello there")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done, "{:?}", setup.reports());
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.system", "pi.assistant"]);
    assert_eq!(Some(entries[0].id), settled.entry);
    assert_eq!(Some(entries[2].id), settled.answer);
    assert_eq!(text_of(entries[2].model.as_ref().and_then(|model| model.first())).as_deref(), Some("Hello there"));
    let Some(Message::System(system)) = entries[1].model.as_ref().and_then(|model| model.first()) else {
        panic!("expected a system message");
    };
    assert_eq!(serde_json::to_value(&system.sections).unwrap(), json!({ "preamble": "You are helpful." }));
    assert_eq!(live(&harness, &root).await, empty());
    let tasks = conversation_tasks(&harness, &root).await;
    assert_eq!(tasks[0].kind, "pi.generation");
    // Entries written by the generation are attributed to it; the admitted user entry is not task work.
    let by: Vec<_> = entries.iter().map(|entry| entry.by_task_id).collect();
    assert_eq!(by, [None, Some(tasks[0].id), Some(tasks[0].id)]);
    assert_eq!(
        serde_json::to_value(&tasks[0].state).unwrap(),
        json!({ "status": "terminal", "outcome": { "status": "completed", "result": { "entryId": settled.answer } } })
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn still_ends_a_run_whose_input_something_else_already_settled() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (step, reached) = unanswered();
    setup.faux.set_responses([step]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    reached.notified().await;
    let id = submission.id;
    let settle = move |tx: pi_durable::session::Tx| async move {
        let settlement = pi_durable::SubmissionSettlement::Unanswered { reason: "withdrawn".into(), detail: None };
        tx.settle_submission(id, settlement)
    };
    root.commit(settle, &ctx()).await.unwrap();
    let task_id = run_task(&harness, &root).await;
    harness.abort_task(task_id, &ctx()).await.unwrap();
    let settled = harness.wait_for_task(task_id, &ctx()).await.unwrap();
    assert!(matches!(settled.state.outcome(), Some(TaskOutcome::Aborted { .. })));
    let status = submission.status(&ctx()).await.unwrap();
    assert_eq!((status.status, status.reason.as_deref()), (SubmissionStatus::Unanswered, Some("withdrawn")));
    // A run ends without an answer even when its inputs were settled already.
    assert_eq!(live(&harness, &root).await, failed(task_id, submission.id, "aborted"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_with_no_model_when_no_model_is_configured_or_the_model_is_unknown() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let plain = harness.create_conversation(CreateOptions::default(), &ctx()).await.unwrap();
    harness.resume().unwrap();
    let unset = plain.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((unset.status, unset.reason.as_deref()), (SubmissionStatus::Unanswered, Some("no_model")));

    let missing = ModelRef { provider: "faux".into(), model_id: "missing".into() };
    root.configure(AgentChange::model(missing), &ctx()).await.unwrap();
    let unknown = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((unknown.status, unknown.reason.as_deref()), (SubmissionStatus::Unanswered, Some("no_model")));
    assert!(unknown.entry.is_some());
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user"]);
    let tasks = conversation_tasks(&harness, &root).await;
    assert_eq!(
        serde_json::to_value(&tasks[0].state).unwrap(),
        json!({
            "status": "terminal",
            "outcome": {
                "status": "failed",
                "error": { "message": "Model faux/missing is not available", "detail": { "reason": "no_model" } }
            }
        })
    );
    let failure = json!({
        "taskId": tasks[0].id,
        "inputs": [unknown.id],
        "reason": "no_model",
        "detail": "Model faux/missing is not available",
    });
    assert_eq!(live(&harness, &root).await, Some(Json::from(json!({ "runFailure": failure }))));
    let plain_failure = live(&harness, &plain).await.unwrap().to_serde()["runFailure"].clone();
    assert_eq!(
        (&plain_failure["reason"], &plain_failure["detail"]),
        (&json!("no_model"), &json!("No model is configured"))
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn retries_a_retryable_error_after_a_durable_backoff_and_then_answers() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    add_section(&setup.registry, "preamble", "p", Some(false));
    setup.faux.set_responses([error_503(), answer("recovered")]);
    setup.settings.lock().unwrap().retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(3),
        base_delay_ms: Some(1),
        ..Default::default()
    });
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done, "{:?}", setup.reports());
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.system", "pi.assistant", "pi.assistant"]);
    let Some(Message::Assistant(failed)) = entries[2].model.as_ref().and_then(|model| model.first()) else {
        panic!("expected an assistant message");
    };
    assert_eq!(failed.stop_reason, StopReason::Error);
    assert_eq!(live(&harness, &root).await, empty());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_with_model_error_once_retries_are_exhausted() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    setup.faux.set_responses([error_503(), error_503()]);
    setup.settings.lock().unwrap().retry = Some(RetryOverrides {
        enabled: Some(true),
        max_retries: Some(1),
        base_delay_ms: Some(1),
        ..Default::default()
    });
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(kinds(&all_entries(&root).await), ["pi.user", "pi.assistant", "pi.assistant"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_a_retryable_error_without_retrying_when_the_retry_policy_is_disabled() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    setup.faux.set_responses([error_503(), answer("unused")]);
    setup.settings.lock().unwrap().retry = Some(RetryOverrides { enabled: Some(false), ..Default::default() });
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(setup.faux.get_pending_response_count(), 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn fails_a_non_retryable_error_without_retrying() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("invalid request".into()),
        ..FauxMessageOptions::default()
    };
    setup.faux.set_responses([faux_assistant_message(Vec::new(), options), answer("unused")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let settled = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(setup.faux.get_pending_response_count(), 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn converts_the_committed_partial_when_aborted_during_streaming() {
    let setup = chat_setup(RegisterFauxProviderOptions {
        tokens_per_second: Some(20.0),
        token_size: Some(pi_durable::testing::faux::FauxTokenSize { min: Some(1), max: Some(1) }),
        ..RegisterFauxProviderOptions::default()
    });
    setup.faux.set_responses([answer(&"x".repeat(400))]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    harness.resume().unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let task_id = run_task(&harness, &root).await;
    let partial_text = || {
        let (harness, root) = (harness.clone(), root.clone());
        async move {
            let value = live(&harness, &root).await?;
            let message: Message = value.get("generation")?.get("message")?.deserialize_into().ok()?;
            text_of(Some(&message))
        }
    };
    wait_for(|| async { partial_text().await.is_some() }).await;
    let partial = partial_text().await.unwrap();
    assert_eq!(harness.abort_task(task_id, &ctx()).await.unwrap(), AbortTaskResult::Marked);
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    let entries = all_entries(&root).await;
    assert_eq!(kinds(&entries), ["pi.user", "pi.assistant"]);
    let converted = entries[1].model.as_ref().and_then(|model| model.first());
    let Some(Message::Assistant(message)) = converted else { panic!("expected an assistant message") };
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert!(text_of(converted).unwrap().starts_with(&partial));
    assert_eq!(live(&harness, &root).await, failed(task_id, submission.id, "aborted"));
    let task = harness.wait_for_task(task_id, &ctx()).await.unwrap();
    assert_eq!(serde_json::to_value(task.state.outcome()).unwrap(), json!({ "status": "aborted" }));
    harness.close(&ctx()).await.unwrap();
}

// "rejects a registry without the built-in tasks": a Rust `RegistrySnapshot` always holds the built-in tasks, so
// no registry reader can lack them; `Harness::open` keeps the check.
