//! Upstream cases on what a continue answers: a failed run, an aborted tool round, a context ending in system
//! messages, and answers cut off at their output limit.

use pi_durable::storage::MemoryStorage;

use super::support::chat::{add_tool, call, no_parameters};
use super::support::task_support::aborted;
use super::*;

#[tokio::test]
async fn answers_a_run_that_failed_without_a_new_user_message() {
    let setup = chat_setup(Default::default());
    let seen = Seen::default();
    setup.faux.set_responses([failure(), answering("recovered", &seen)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let first = root.submit(SubmissionDraft::input("Analyze this"), &ctx()).await.unwrap();
    let failed = first.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&failed), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(live(&harness, &root).await["runFailure"]["reason"], "model_error");

    let resumed = root.submit(SubmissionDraft::continue_run(), &ctx()).await.unwrap();
    assert_eq!(resumed.status(&ctx()).await.unwrap().status, SubmissionStatus::Placed);
    let settled = resumed.wait(&ctx()).await.unwrap();
    assert_eq!(settled.kind, SubmissionType::Continue);
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("recovered"));
    assert_eq!(last_roles(&seen, 1), ["user"]);
    // The failed input stays unanswered; no user entry was added.
    assert_eq!(first.status(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    assert_eq!(user_entries(&root).await, 1);
    assert!(live(&harness, &root).await.get("runFailure").is_none());
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn answers_the_tool_results_of_a_round_aborted_midway() {
    let setup = chat_setup(Default::default());
    let started = Deferred::default();
    let entered = started.clone();
    add_tool(
        &setup,
        define_tool("wait", "Waits until aborted", no_parameters(), move |_, _, context| {
            let entered = entered.clone();
            async move {
                entered.resolve();
                let signal = context.abort_signal().unwrap().clone();
                aborted(signal.clone()).await;
                Err(signal.reason().map_or_else(DurableError::aborted, DurableError::from))
            }
        }),
    );
    let seen = Seen::default();
    setup.faux.set_responses([call("wait", "c1"), answering("after the abort", &seen)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let first = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap();
    started.wait().await;
    root.abort(&ctx(), false).await.unwrap();
    let failed = first.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&failed), (SubmissionStatus::Unanswered, Some("aborted")));
    let settled = continued(&root).await;
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("after the abort"));
    assert_eq!(last_roles(&seen, 1), ["toolResult"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn skips_system_messages_when_deciding_there_is_something_to_answer() {
    let setup = chat_setup(Default::default());
    let seen = Seen::default();
    let first_answer: FauxResponseStep = faux_assistant_message("first answer", FauxMessageOptions::default()).into();
    setup.faux.set_responses([first_answer, failure(), answering("resumed", &seen)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    let instructions = |text: &str| AgentChange { instructions: Change::Set(text.into()), ..AgentChange::default() };
    root.configure(instructions("Be thorough."), &ctx()).await.unwrap();
    root.submit(SubmissionDraft::input("one"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    // Changed instructions append a system delta after the next user message, before its request.
    root.configure(instructions("Be brief."), &ctx()).await.unwrap();
    let second = root.submit(SubmissionDraft::input("two"), &ctx()).await.unwrap();
    let failed = second.wait(&ctx()).await.unwrap();
    assert_eq!(reason(&failed), (SubmissionStatus::Unanswered, Some("model_error")));
    let messages = root.context(&ctx(), None).await.unwrap().messages;
    assert_eq!(messages.last().map(|message| message.role()), Some("system"));
    let settled = continued(&root).await;
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("resumed"));
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn continues_an_answer_with_tool_calls_cut_off_at_its_output_limit_which_ends_in_their_missing_results() {
    let setup = chat_setup(Default::default());
    let seen = Seen::default();
    setup.faux.set_responses([cut_off("calling", &[("echo", "c1")]), answering("resumed", &seen)]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    // The run answered: a cut-off answer is an answer, and its calls never ran.
    let first = root.submit(SubmissionDraft::input("go"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    assert_eq!(first.status, SubmissionStatus::Done);
    let settled = continued(&root).await;
    assert_eq!(answer_text(&root, &settled).await.as_deref(), Some("resumed"));
    assert_eq!(last_roles(&seen, 2), ["assistant", "toolResult"]);
    harness.close(&ctx()).await.unwrap();
}

#[tokio::test]
async fn does_not_continue_a_text_answer_cut_off_at_its_output_limit() {
    let setup = chat_setup(Default::default());
    setup.faux.set_responses([cut_off("cut", &[])]);
    let (harness, root) = open_chat(Arc::new(MemoryStorage::new()), &setup).await;
    root.submit(SubmissionDraft::input("write a lot"), &ctx()).await.unwrap().wait(&ctx()).await.unwrap();
    let settled = continued(&root).await;
    assert_eq!(reason(&settled), (SubmissionStatus::Unanswered, Some("nothing_to_answer")));
    assert_eq!(setup.faux.state().call_count, 1);
    harness.close(&ctx()).await.unwrap();
}
