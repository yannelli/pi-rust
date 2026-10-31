//! Port of upstream `test/harness-run-failure.test.ts`: `pi.live.runFailure`, why the newest run ended without an
//! answer, as state a renderer shows.

mod support;

use std::sync::Arc;

use pi_ai::StopReason;
use pi_durable::SubmissionStatus;
use pi_durable::harness::live::LIVE_DOC;
use pi_durable::harness::*;
use pi_durable::storage::MemoryStorage;
use pi_durable::testing::faux::{
    FauxMessageOptions, FauxResponseStep, RegisterFauxProviderOptions, faux_assistant_message,
};
use serde_json::{Value, json};
use support::Deferred;
use support::chat::{add_tool, answer, call, no_parameters};
use support::harness_support::*;
use tokio::sync::Notify;

async fn live(harness: &Harness, conversation: &Conversation) -> Value {
    let value = harness.snapshot(&LIVE_DOC, conversation.id, &ctx()).await.unwrap();
    value.map(|value| value.to_serde()).unwrap_or_else(|| json!({}))
}

fn invalid_request() -> FauxMessageOptions {
    FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("Invalid request".into()),
        ..FauxMessageOptions::default()
    }
}

/// A response that fails for good once `gate` opens; `reached` fires when it was requested.
fn failing_once() -> (FauxResponseStep, Arc<Notify>, Deferred) {
    let (reached, gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (reached.clone(), gate.clone());
    let step = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held) = (notify.clone(), held.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            Ok(faux_assistant_message(Vec::new(), invalid_request()))
        })
    });
    (step, reached, gate)
}

#[tokio::test]
async fn records_a_model_error_with_its_text_and_a_later_run_clears_it() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    setup.faux.set_responses([faux_assistant_message(Vec::new(), invalid_request()).into(), answer("fine")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!("Invalid request")));
    let failure = live(&harness, &root).await["runFailure"].clone();
    assert!(failure["taskId"].is_u64(), "{failure}");
    let expected = json!({
        "taskId": failure["taskId"],
        "inputs": [submission.id],
        "reason": "model_error",
        "detail": "Invalid request",
    });
    assert_eq!(failure, expected);
    let next = root.submit(SubmissionDraft::input("again"), &ctx()).await.unwrap();
    // The commit that starts the next run removes it.
    assert!(live(&harness, &root).await.get("runFailure").is_none());
    assert_eq!(next.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(live(&harness, &root).await, json!({}));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_a_missing_model_with_which_one() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let gone = ModelRef { provider: "faux".into(), model_id: "gone".into() };
    root.configure(AgentChange::model(gone), &ctx()).await.unwrap();
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("no_model")));
    // The settlement is unchanged: no detail.
    assert!(settled.detail.is_none());
    let failure = live(&harness, &root).await["runFailure"].clone();
    assert_eq!(failure["inputs"], json!([submission.id]));
    assert_eq!(
        (&failure["reason"], &failure["detail"]),
        (&json!("no_model"), &json!("Model faux/gone is not available"))
    );
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_an_abort() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (busy, reached) = unanswered();
    setup.faux.set_responses([busy]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let submission = root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap();
    reached.notified().await;
    let task_id = live(&harness, &root).await["run"]["taskId"].clone();
    root.abort(&ctx(), false).await.unwrap();
    let settled = submission.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("aborted")));
    let failure = live(&harness, &root).await["runFailure"].clone();
    assert_eq!(failure, json!({ "taskId": task_id, "inputs": [submission.id], "reason": "aborted" }));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn records_a_reset_that_cut_the_run_before_an_answer_kept_by_the_run_its_queued_input_starts() {
    let setup = chat_setup(RegisterFauxProviderOptions::default());
    let (gate, started) = (Deferred::default(), Arc::new(Notify::new()));
    let (held, entered) = (gate.clone(), started.clone());
    add_tool(
        &setup,
        define_tool("hold", "Waits", no_parameters(), move |_, _, _| {
            let (held, entered) = (held.clone(), entered.clone());
            async move {
                entered.notify_one();
                held.wait().await;
                Ok(ToolExecutionResult::default())
            }
        }),
    );
    let (second, reached) = unanswered();
    setup.faux.set_responses([call("hold", "c1"), second]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let first = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.notified().await;
    root.reset(None, &ctx()).await.unwrap();
    let draft = SubmissionDraft::input("next").with_when_busy(WhenBusy::FollowUp);
    let follow_up = root.submit(draft, &ctx()).await.unwrap();
    gate.resolve();
    let settled = first.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("reset")));
    reached.notified().await;
    let state = live(&harness, &root).await;
    // The follow-up's run started in the commit that ended the first; the failure stays until a later run starts.
    assert_eq!(state["run"]["inputs"], json!([follow_up.id]));
    assert_eq!((&state["runFailure"]["inputs"], &state["runFailure"]["reason"]), (&json!([first.id]), &json!("reset")));
    root.abort(&ctx(), false).await.unwrap();
    let failure = live(&harness, &root).await["runFailure"].clone();
    assert_eq!((&failure["inputs"], &failure["reason"]), (&json!([follow_up.id]), &json!("aborted")));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_the_failure_while_a_run_continues_the_queue_in_the_same_commit_and_replaces_it_with_the_next() {
    for policy in [AfterFailedRun::Continue, AfterFailedRun::Keep] {
        let setup = chat_setup(RegisterFauxProviderOptions::default());
        setup.settings.lock().unwrap().after_failed_run = Some(policy);
        let (failing, failed, gate) = failing_once();
        let (busy, reached) = unanswered();
        setup.faux.set_responses([failing, busy, answer("third")]);
        let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
        let first = root.submit(SubmissionDraft::input("first"), &ctx()).await.unwrap();
        failed.notified().await;
        let draft = SubmissionDraft::input("second").with_when_busy(WhenBusy::FollowUp);
        let second = root.submit(draft, &ctx()).await.unwrap();
        gate.resolve();
        first.wait(&ctx()).await.unwrap();
        if policy == AfterFailedRun::Continue {
            reached.notified().await;
            let state = live(&harness, &root).await;
            assert_eq!(state["run"]["inputs"], json!([second.id]));
            let failure = &state["runFailure"];
            assert_eq!((&failure["inputs"], &failure["reason"]), (&json!([first.id]), &json!("model_error")));
        } else {
            harness.wait_for_idle(&ctx()).await.unwrap();
            assert_eq!(live(&harness, &root).await["runFailure"]["inputs"], json!([first.id]));
        }
        harness.close(&ctx()).await.unwrap();
    }
}
