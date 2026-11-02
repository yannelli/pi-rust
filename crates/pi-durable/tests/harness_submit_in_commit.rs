//! Port of upstream `test/harness-submit-in-commit.test.ts` as of `6a2825113`: admission for hosts that must not
//! start runs, `WhenIdle::Reject` (#11149).

mod support;

use std::sync::{Arc, Mutex};

use pi_ai::StopReason;
use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
};
use pi_durable::{ConversationId, ConversationOwnership, DurableError, SubmissionStatus, SubmissionType};
use serde_json::{Value, json};
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::*;
use support::{ControlledStorage, Deferred};
use tokio::sync::Notify;

fn setup() -> ChatSetup {
    chat_setup(RegisterFauxProviderOptions::default())
}

fn steer_unless_idle(text: &str) -> SubmissionDraft {
    SubmissionDraft::input(text).with_when_busy(WhenBusy::Steer).with_when_idle(WhenIdle::Reject)
}

async fn run_of(harness: &Harness, id: ConversationId) -> Option<Value> {
    let live = harness.snapshot(&LIVE_DOC, id, &ctx()).await.unwrap();
    live.and_then(|live| live.get("run").map(|run| run.to_serde()))
}

#[tokio::test]
async fn rejects_input_to_an_idle_conversation_without_writing() {
    let storage = Arc::new(ControlledStorage::default());
    let (harness, root) = open_chat(storage.clone(), &setup()).await;
    let commits = storage.commit_count();
    let error = root.submit(steer_unless_idle("hi"), &ctx()).await.err().unwrap();
    assert!(matches!(error, DurableError::ConversationIdle(id) if id == root.id), "{error:?}");
    assert_eq!((error.name(), error.to_string()), ("ConversationIdle", format!("Conversation {} is idle", root.id)));
    assert_eq!(storage.commit_count(), commits);
    assert_eq!(run_of(&harness, root.id).await, None);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_input_to_an_idle_conversation_that_keeps_queued_items_after_a_failed_run() {
    let setup = setup();
    let (requested, failing) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (requested.clone(), failing.clone());
    let failure = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held) = (notify.clone(), held.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            let options = FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some("Invalid request".into()),
                ..FauxMessageOptions::default()
            };
            Ok(faux_assistant_message(Vec::new(), options))
        })
    });
    setup.faux.set_responses([failure]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let first = root.submit(SubmissionDraft::input("first"), &ctx()).await.unwrap();
    requested.notified().await;
    let queued = root.submit(SubmissionDraft::input("queued"), &ctx()).await.unwrap();
    failing.resolve();
    let settled = first.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    harness.wait_for_idle(&ctx()).await.unwrap();
    let error = root.submit(steer_unless_idle("steer"), &ctx()).await.err().unwrap();
    assert!(matches!(error, DurableError::ConversationIdle(_)), "{error:?}");
    assert_eq!(queued.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    assert_eq!(run_of(&harness, root.id).await, None);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn steers_a_working_run() {
    let setup = setup();
    let (step, reached) = unanswered();
    setup.faux.set_responses([step]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let first = root.submit(SubmissionDraft::input("work"), &ctx()).await.unwrap();
    reached.notified().await;
    let steer = root.submit(steer_unless_idle("also this"), &ctx()).await.unwrap();
    let record = steer.status(&ctx()).await.unwrap();
    assert_eq!((record.kind, record.status), (SubmissionType::Input, SubmissionStatus::Queued));
    let inbox = harness.snapshot(&INBOX_DOC, root.id, &ctx()).await.unwrap().unwrap().to_serde();
    assert_eq!(inbox["items"], json!([{ "id": steer.id, "mode": "steer", "content": "also this" }]));
    assert_eq!(run_of(&harness, root.id).await.unwrap()["inputs"], json!([first.id]));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn returns_the_submission_of_a_known_request_id_also_once_idle() {
    let setup = setup();
    setup.faux.set_responses([answer("done")]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let first = root.submit(SubmissionDraft::input("work").with_request_id("r1"), &ctx()).await.unwrap();
    first.wait(&ctx()).await.unwrap();
    let again = root.submit(steer_unless_idle("work").with_request_id("r1"), &ctx()).await.unwrap();
    assert_eq!(again.id, first.id);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn rejects_through_a_tool_s_conversation_handle() {
    let setup = setup();
    let other = Arc::new(Mutex::new(None::<ConversationId>));
    let rejection = Arc::new(Mutex::new(None::<DurableError>));
    let (target, sink) = (other.clone(), rejection.clone());
    let nudge = define_tool("nudge", "Steers another conversation", no_parameters(), move |_, api, context| {
        let (target, sink) = (target.clone(), sink.clone());
        async move {
            let id = target.lock().unwrap().unwrap();
            let handle = api.conversation(id, &context).await?.unwrap();
            let error = handle.submit(steer_unless_idle("nudge"), &context).await.err();
            *sink.lock().unwrap() = error;
            Ok(ToolExecutionResult::text("nudged"))
        }
    });
    add_tool(&setup, nudge);
    setup.faux.set_responses([call("nudge", "c1"), answer("ok")]);
    let (harness, root) = open_chat(Arc::new(ControlledStorage::default()), &setup).await;
    let options = CreateOptions::owned(ConversationOwnership::Ownerless).with_agent(AgentChange::model(faux_model()));
    let created = harness.create_conversation(options, &ctx()).await.unwrap();
    *other.lock().unwrap() = Some(created.id);
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let error = rejection.lock().unwrap().take();
    assert!(matches!(error, Some(DurableError::ConversationIdle(id)) if id == created.id), "{error:?}");
    assert_eq!(run_of(&harness, created.id).await, None);
    harness.close(&ctx()).await.unwrap();
}

#[test]
fn a_write_draft_takes_no_idle_policy() {
    let write = SubmissionDraft::write(pi_durable::EntryDraft::new("note")).with_when_idle(WhenIdle::Reject);
    assert!(matches!(write, SubmissionDraft::Write { .. }));
    assert_eq!(serde_json::to_value(WhenIdle::Reject).unwrap(), json!("reject"));
}
