//! The upstream "compaction and the inbox" cases.

use pi_ai::{UserContent, UserMessage};
use pi_durable::{ContextEdit, ContextEditAction, EntryDraft};

use super::chat::*;

pub async fn queued_summary(chat: &Chat, content: &str) -> Submission {
    chat.faux.summary(answer(content));
    let task = chat.compact(None).await;
    chat.placed(task).await
}

pub struct Busy {
    pub input: Submission,
    gate: Deferred,
}

impl Busy {
    pub fn release(&self) {
        self.gate.resolve();
    }
}

/// Start a run whose answer `reply` waits until released.
pub async fn busy(chat: &Chat, reply: AssistantMessage) -> Busy {
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.agent(gated(&gate, reply, Some(&reached)));
    let input = chat.submit("busy").await;
    reached.wait().await;
    Busy { input, gate }
}

#[tokio::test]
async fn places_a_reset_queued_after_the_summary_last_and_makes_a_summary_queued_after_a_reset_stale() {
    for summary_first in [true, false] {
        let chat = open(OpenOptions::default()).await;
        chat.history().await;
        let run = busy(&chat, answer("done")).await;
        let submission = if summary_first {
            let submission = queued_summary(&chat, "SUMMARY").await;
            chat.root.reset(None, &ctx()).await.unwrap();
            submission
        } else {
            chat.root.reset(None, &ctx()).await.unwrap();
            queued_summary(&chat, "SUMMARY").await
        };
        run.release();
        run.input.wait(&ctx()).await.unwrap();
        let settled = submission.wait(&ctx()).await.unwrap();
        let expected = if summary_first { SubmissionStatus::Done } else { SubmissionStatus::Unanswered };
        assert_eq!(settled.status, expected);
        assert_eq!(chat.kinds().await.last().map(String::as_str), Some("pi.reset"));
        let head = chat.root.context(&ctx(), None).await.unwrap().head.unwrap();
        assert_eq!(head.kind, "pi.reset");
        chat.close().await;
    }
}

#[tokio::test]
async fn places_a_summary_left_queued_by_a_failed_run_at_the_next_submission_before_its_input() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let run = busy(&chat, failure("bad request")).await;
    let submission = queued_summary(&chat, "SUMMARY").await;
    run.release();
    assert_eq!(run.input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Unanswered);
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Queued);
    chat.turn("again", "ok").await;
    assert_eq!(submission.status(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    let request = chat.faux.last_agent_request();
    assert!(first_user_text(&request).contains("SUMMARY"));
    assert!(request.iter().any(|message| user_text(Some(message)) == "again"));
    chat.close().await;
}

#[tokio::test]
async fn keeps_the_full_retry_budget_after_an_overflow_compaction() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.set_policy(CompactionOverrides { enabled: Some(true), ..manual() });
    chat.faux.summary(summary());
    for step in [failure("prompt is too long"), failure("overloaded"), failure("overloaded"), answer("finally")] {
        chat.faux.agent(step);
    }
    let input = chat.submit(&text("u4", 100)).await;
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    chat.close().await;
}

#[tokio::test]
async fn loses_an_application_edit_placed_while_a_compaction_summarizes() {
    // Upstream cites spec section 12 for this loss.
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let u1 = all_entries(&chat.root).await[0].id;
    let (gate, reached) = (Deferred::default(), Deferred::default());
    chat.faux.summary(gated(&gate, summary(), Some(&reached)));
    let task = chat.compact(None).await;
    reached.wait().await;
    let redacted = Message::User(UserMessage { content: UserContent::Text("REDACTED".into()), timestamp: 0 });
    let edit = ContextEdit { target: u1, action: ContextEditAction::Replace, messages: Some(vec![redacted]) };
    let entry = EntryDraft { edits: Some(vec![edit]), ..EntryDraft::new("app.redact") };
    chat.root.submit(SubmissionDraft::write(entry), &ctx()).await.unwrap();
    gate.resolve();
    chat.result(task).await;
    // The summary was made from the unredacted entry, and the edit's target left the range.
    assert!(user_text(chat.faux.summary_requests()[0].messages.get(1)).contains("[User]: u1 "));
    let messages = chat.root.context(&ctx(), None).await.unwrap().messages;
    assert!(!messages.iter().any(|message| user_text(Some(message)) == "REDACTED"));
    chat.close().await;
}

#[tokio::test]
async fn keeps_the_kept_entries_mounted_in_the_conversation_view() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    let state = Arc::new(chat.root.view_state(&ctx()).await.unwrap());
    chat.faux.summary(summary());
    let task = chat.compact(None).await;
    chat.result(task).await;
    let probe = state.clone();
    wait_for(move || {
        let first =
            probe.value().get("entries").and_then(|entries| entries.to_serde()[0]["kind"].as_str().map(str::to_owned));
        async move { first.as_deref() == Some("pi.compaction") }
    })
    .await;
    let mounted = state.value().get("entries").unwrap().to_serde();
    let entries = chat.root.context(&ctx(), None).await.unwrap().entries;
    assert_eq!(Json::from(mounted), Json::from(serde_json::to_value(entries).unwrap()));
    state.dispose().unwrap();
    chat.close().await;
}
