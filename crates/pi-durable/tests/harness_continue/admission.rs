//! Upstream cases on admission: busy rejection after request ID deduplication, nothing to answer, and the inbox
//! items a failed run kept, which a continue places first.

use pi_durable::harness::inbox::INBOX_DOC;
use pi_durable::storage::MemoryStorage;

use super::*;

async fn inbox_items(harness: &Harness, conversation: &Conversation) -> Vec<Value> {
    let value = harness.snapshot(&INBOX_DOC, conversation.id, &ctx()).await.unwrap();
    let items = value.map(|value| value.to_serde()["items"].clone());
    items.and_then(|items| items.as_array().cloned()).unwrap_or_default()
}

#[tokio::test]
async fn is_rejected_while_busy_after_request_id_deduplication() {
    let setup = chat_setup(Default::default());
    let (busy, reached) = unanswered();
    setup.faux.set_responses([failure(), busy]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let resumed = root.submit(SubmissionDraft::continue_run().with_request_id("resume-1"), &ctx()).await.unwrap();
    reached.notified().await;
    let rejected = root.submit(SubmissionDraft::continue_run(), &ctx()).await;
    assert!(matches!(rejected, Err(DurableError::ConversationBusy(id)) if id == root.id));
    // The same request ID returns the existing submission instead of rejecting.
    let again = root.submit(SubmissionDraft::continue_run().with_request_id("resume-1"), &ctx()).await.unwrap();
    assert_eq!(again.id, resumed.id);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn ends_without_a_request_when_there_is_nothing_to_answer() {
    let setup = chat_setup(Default::default());
    setup.faux.set_responses([faux_assistant_message("answered", FauxMessageOptions::default())]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let empty = continued(&root).await;
    assert_eq!(empty.kind, SubmissionType::Continue);
    assert_eq!(reason(&empty), (SubmissionStatus::Unanswered, Some("nothing_to_answer")));
    root.submit(SubmissionDraft::input("hi"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let calls = setup.faux.state().call_count;
    // A context ending in an answer has nothing to continue.
    let after = continued(&root).await;
    assert_eq!(reason(&after), (SubmissionStatus::Unanswered, Some("nothing_to_answer")));
    assert_eq!(setup.faux.state().call_count, calls);
    assert_eq!(live(&harness, &root).await["runFailure"]["reason"], "nothing_to_answer");
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_a_reset_a_failed_run_kept_and_finds_nothing_to_answer_in_the_context_it_starts() {
    let setup = chat_setup(Default::default());
    let (reached, gate) = (Deferred::default(), Deferred::default());
    setup.faux.set_responses([held_failure(&reached, &gate)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let first = root.submit(SubmissionDraft::input("first"), &ctx()).await.unwrap();
    reached.wait().await;
    root.reset(None, &ctx()).await.unwrap();
    gate.resolve();
    let failed = first.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&failed), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(inbox_items(&harness, &root).await.len(), 1);

    let settled = continued(&root).await;
    assert_eq!(reason(&settled), (SubmissionStatus::Unanswered, Some("nothing_to_answer")));
    assert_eq!(inbox_items(&harness, &root).await, Vec::<Value>::new());
    let messages = root.context(&ctx(), None).await.unwrap().messages;
    assert_eq!(messages.iter().filter(|message| message.role() != "system").count(), 0);
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn starts_the_inputs_queued_behind_a_continue_with_nothing_to_answer_under_after_failed_run_continue() {
    let setup = chat_setup(Default::default());
    setup.settings.lock().unwrap().after_failed_run = Some(AfterFailedRun::Continue);
    let answer = |text: &str| faux_assistant_message(text, FauxMessageOptions::default());
    setup.faux.set_responses([answer("answered"), answer("second answered")]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("one"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let (context, gate) = (ctx(), Deferred::default());
    let held = gate.clone();
    // Commits enter the line when first polled, and the run a continue starts only after its admission: the commit
    // held behind both admissions keeps that run from ending until the queued input's stored status is read.
    let hold = root.commit(
        move |_: Tx| async move {
            held.wait().await;
            Ok(())
        },
        &context,
    );
    let admit = async {
        let draft = SubmissionDraft::input("two").with_when_busy(WhenBusy::FollowUp);
        let queued = root.submit(draft, &context).await.unwrap();
        let stored = harness.session().storage().submission(queued.id, &context).await.unwrap();
        assert_eq!(stored.map(|record| record.status), Some(SubmissionStatus::Queued));
        gate.resolve();
        queued
    };
    let (resumed, queued, released) =
        futures::join!(root.submit(SubmissionDraft::continue_run(), &context), admit, hold);
    released.unwrap();
    let resumed = resumed.unwrap();
    let ended = resumed.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&ended), (SubmissionStatus::Unanswered, Some("nothing_to_answer")));
    let settled = queued.wait(&ctx()).await.unwrap();
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("second answered"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn places_inputs_a_failed_run_left_queued_first_and_they_join_its_run() {
    let setup = chat_setup(Default::default());
    let (reached, gate) = (Deferred::default(), Deferred::default());
    let both: FauxResponseStep = faux_assistant_message("both handled", FauxMessageOptions::default()).into();
    setup.faux.set_responses([held_failure(&reached, &gate), both]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("first"), &ctx()).await.unwrap();
    reached.wait().await;
    let draft = SubmissionDraft::input("second").with_when_busy(WhenBusy::FollowUp);
    let queued = root.submit(draft, &ctx()).await.unwrap();
    gate.resolve();
    harness.wait_for_idle(&ctx()).await.unwrap();
    assert_eq!(queued.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);

    let resumed = root.submit(SubmissionDraft::continue_run(), &ctx()).await.unwrap();
    assert_eq!(live(&harness, &root).await["run"]["inputs"], json!([queued.id, resumed.id]));
    let (by_queued, by_continue) = (queued.wait(&ctx()).await.unwrap(), resumed.wait(&ctx()).await.unwrap());
    assert_eq!(answer_text(&root, &by_continue).await.as_deref(), Some("both handled"));
    assert_eq!((by_queued.status, by_queued.answer), (SubmissionStatus::Done, by_continue.answer));
    harness.close(&ctx()).await.unwrap();
}
