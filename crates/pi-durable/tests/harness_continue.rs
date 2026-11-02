//! Port of upstream `test/harness-continue.test.ts`: `SubmissionDraft::Continue`, a run without a new user message
//! (upstream #11043).

mod support;

#[path = "harness_continue/admission.rs"]
mod admission;
#[path = "harness_continue/callers.rs"]
mod callers;
#[path = "harness_continue/records.rs"]
mod records;
#[path = "harness_continue/runs.rs"]
mod runs;

use std::sync::{Arc, Mutex};

use pi_ai::{AssistantMessage, StopReason};
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::session::Tx;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, faux_assistant_message, faux_text, faux_tool_call_with_id,
};
use pi_durable::*;
use serde_json::{Value, json};
use support::Deferred;
use support::harness_support::*;

/// Roles of each request's context, in request order.
type Seen = Arc<Mutex<Vec<Vec<String>>>>;

fn failed_answer() -> AssistantMessage {
    let options = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("Invalid request".into()),
        ..FauxMessageOptions::default()
    };
    faux_assistant_message(Vec::new(), options)
}

fn failure() -> FauxResponseStep {
    failed_answer().into()
}

/// A response that records the roles of the context it was asked with, then answers `text`.
fn answering(text: &str, seen: &Seen) -> FauxResponseStep {
    let (text, seen) = (text.to_owned(), seen.clone());
    FauxResponseStep::factory(move |request, _, _, _| {
        seen.lock().unwrap().push(request.messages.iter().map(|message| message.role().to_owned()).collect());
        faux_assistant_message(vec![faux_text(text.as_str())], FauxMessageOptions::default())
    })
}

/// A response that fails for good once `gate` opens; `reached` opens when it was requested.
fn held_failure(reached: &Deferred, gate: &Deferred) -> FauxResponseStep {
    let (reached, gate) = (reached.clone(), gate.clone());
    FauxResponseStep::async_factory(move |_, _, _, _| {
        let (reached, gate) = (reached.clone(), gate.clone());
        Box::pin(async move {
            reached.resolve();
            gate.wait().await;
            Ok(failed_answer())
        })
    })
}

/// A text answer cut off at its output limit, with `calls` as `(name, id)` tool calls after the text.
fn cut_off(text: &str, calls: &[(&str, &str)]) -> FauxResponseStep {
    let mut blocks = vec![faux_text(text)];
    blocks.extend(calls.iter().map(|(name, id)| faux_tool_call_with_id(*name, json!({}), *id)));
    faux_assistant_message(blocks, FauxMessageOptions::stop_reason(StopReason::Length)).into()
}

fn last_roles(seen: &Seen, count: usize) -> Vec<String> {
    let requests = seen.lock().unwrap();
    let last = requests.last().expect("a recorded request");
    last[last.len().saturating_sub(count)..].to_vec()
}

async fn answer_text(conversation: &Conversation, settled: &SubmissionRecord) -> Option<String> {
    assert_eq!(settled.status, SubmissionStatus::Done, "{settled:?}");
    assert_ne!(settled.kind, SubmissionType::Write);
    let answer = settled.answer.expect("an answer");
    let read = conversation.commit(move |tx: Tx| async move { tx.entry(answer).await }, &ctx()).await.unwrap();
    text_of(read.and_then(|entry| entry.model).as_ref().and_then(|model| model.first()))
}

async fn user_entries(conversation: &Conversation) -> usize {
    all_entries(conversation).await.iter().filter(|entry| entry.kind == "pi.user").count()
}

async fn live(harness: &Harness, conversation: &Conversation) -> Value {
    let value = harness.snapshot(&LIVE_DOC, conversation.id, &ctx()).await.unwrap();
    value.map(|value| value.to_serde()).unwrap_or_else(|| json!({}))
}

async fn continued(conversation: &Conversation) -> SubmissionRecord {
    let submission = conversation.submit(SubmissionDraft::continue_run(), &ctx()).await.unwrap();
    submission.wait(&ctx()).await.unwrap()
}

fn reason(record: &SubmissionRecord) -> (SubmissionStatus, Option<&str>) {
    (record.status, record.reason.as_deref())
}
