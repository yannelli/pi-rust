//! Port of upstream `test/harness-failed-run-inbox.test.ts`: what a failed run does with the inputs queued behind it
//! (`HarnessSettings::after_failed_run`).

use pi_durable::harness::submissions::Submission;

use super::*;

/// A response that fails for good once `gate` opens; `reached` fires when it was requested.
fn failing_once() -> (FauxResponseStep, Arc<Notify>, Deferred) {
    let error = FauxMessageOptions {
        stop_reason: Some(StopReason::Error),
        error_message: Some("Invalid request".into()),
        ..FauxMessageOptions::default()
    };
    let (reached, gate) = (Arc::new(Notify::new()), Deferred::default());
    let (notify, held) = (reached.clone(), gate.clone());
    let step = FauxResponseStep::async_factory(move |_, _, _, _| {
        let (notify, held, error) = (notify.clone(), held.clone(), error.clone());
        Box::pin(async move {
            notify.notify_one();
            held.wait().await;
            Ok(faux_assistant_message(Vec::new(), error))
        })
    });
    (step, reached, gate)
}

fn with_policy(policy: Option<AfterFailedRun>) -> ChatSetup {
    let setup = setup();
    setup.settings.lock().unwrap().after_failed_run = policy;
    setup
}

/// A run that fails with an input queued behind it, under `policy`; returns the queued input.
async fn fail_with_queued_input(policy: Option<AfterFailedRun>) -> (Harness, Conversation, Submission) {
    let setup = with_policy(policy);
    let (failing, reached, gate) = failing_once();
    setup.faux.set_responses([failing, answer("second answer"), answer("third answer")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let first = root.submit(SubmissionDraft::input("Analyze this report"), &ctx()).await.unwrap();
    reached.notified().await;
    let draft = SubmissionDraft::input("Email me the summary").with_when_busy(WhenBusy::FollowUp);
    let second = root.submit(draft, &ctx()).await.unwrap();
    gate.resolve();
    let settled = first.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    (harness, root, second)
}

#[tokio::test]
async fn inputs_queued_behind_a_failed_run_stay_queued_by_default() {
    let (harness, root, second) = fail_with_queued_input(None).await;
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(second.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    // The next input runs the queue, as before.
    let third = root.submit(SubmissionDraft::input("Anything else?"), &ctx()).await.unwrap();
    assert_eq!(second.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(third.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn inputs_queued_behind_a_failed_run_start_the_next_run_with_continue() {
    let (harness, root, second) = fail_with_queued_input(Some(AfterFailedRun::Continue)).await;
    let settled = second.wait(&ctx()).await.unwrap();
    assert_eq!(settled.status, SubmissionStatus::Done);
    let entries = all_entries(&root).await;
    let answer = entries.iter().find(|entry| Some(entry.id) == settled.answer).unwrap();
    assert_eq!(text_of(answer.model.as_ref().and_then(|model| model.first())).as_deref(), Some("second answer"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn queued_writes_behind_a_failed_run_are_placed_with_continue_and_kept_with_withdraw() {
    for policy in [AfterFailedRun::Continue, AfterFailedRun::Withdraw] {
        let setup = with_policy(Some(policy));
        let (failing, reached, gate) = failing_once();
        setup.faux.set_responses([failing, answer("second answer")]);
        let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
        let first = root.submit(SubmissionDraft::input("first"), &ctx()).await.unwrap();
        reached.notified().await;
        let note = Message::User(pi_ai::UserMessage {
            content: pi_ai::UserContent::Text("a note the model sees".into()),
            timestamp: 1,
        });
        let draft = EntryDraft { model: Some(vec![note]), ..EntryDraft::new("test.note") };
        let write = root.submit(SubmissionDraft::write(draft), &ctx()).await.unwrap();
        assert_eq!(write.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
        gate.resolve();
        first.wait(&ctx()).await.unwrap();
        harness.wait_for_idle(&ctx()).await.unwrap();
        // `Continue` places the write at the failed run's boundary; `Withdraw` withdraws inputs only.
        let expected =
            if policy == AfterFailedRun::Continue { SubmissionStatus::Done } else { SubmissionStatus::Queued };
        assert_eq!(write.status(&ctx()).await.unwrap().status, expected);
        harness.close(&ctx()).await.unwrap();
    }
}

#[tokio::test]
async fn inputs_queued_behind_a_failed_run_settle_unanswered_with_run_failed_with_withdraw() {
    let (harness, _, second) = fail_with_queued_input(Some(AfterFailedRun::Withdraw)).await;
    let settled = second.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("run_failed")));
    harness.close(&ctx()).await.unwrap();
}
