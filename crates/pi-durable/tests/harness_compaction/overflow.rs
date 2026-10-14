//! The upstream "overflow compaction" cases.

use pi_durable::harness::compaction::CompactionDecision;

use super::chat::*;
use super::outcomes::before_compact;

fn enabled() -> CompactionOverrides {
    CompactionOverrides { enabled: Some(true), ..manual() }
}

#[tokio::test]
async fn compacts_and_retries_with_the_same_attempt_leaving_the_error_out_of_the_retry() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.set_policy(enabled());
    chat.faux.summary(summary());
    let attempt = Arc::new(Mutex::new(None));
    chat.faux.agent(failure(OVERFLOW));
    let (probe, sink) = (chat.clone(), attempt.clone());
    chat.faux.agent(Step::Run(Arc::new(move |_| {
        let (chat, sink) = (probe.clone(), sink.clone());
        Box::pin(async move {
            *sink.lock().unwrap() = chat.live().await["generation"]["attempt"].as_u64();
            Ok(answer("fits"))
        })
    })));
    let input = chat.submit(&text("u4", 100)).await;
    assert_eq!(input.wait(&ctx()).await.unwrap().status, SubmissionStatus::Done);
    assert_eq!(*attempt.lock().unwrap(), Some(1));
    let expected = ["pi.user", "pi.assistant", "pi.compaction", "pi.system", "pi.assistant"];
    assert_eq!(tail(&chat.kinds().await, 5), expected);
    let entries = all_entries(&chat.root).await;
    let compaction = entries.iter().find(|record| record.kind == "pi.compaction").unwrap();
    assert_eq!(compaction.data.as_ref().map(Json::to_serde), Some(json!({ "reason": "overflow" })));
    let retry = chat.faux.last_agent_request();
    assert!(user_text(retry.first()).contains("SUMMARY"));
    let errored = |message: &Message| matches!(message, Message::Assistant(a) if a.stop_reason == StopReason::Error);
    assert!(!retry.iter().any(errored));
    chat.close().await;
}

#[tokio::test]
async fn fails_a_second_overflow_with_its_error_entry() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.set_policy(enabled());
    chat.faux.summary(summary());
    chat.faux.agent(failure(OVERFLOW));
    chat.faux.agent(failure(OVERFLOW));
    let input = chat.submit(&text("u4", 100)).await;
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!(OVERFLOW)));
    let kinds = chat.kinds().await;
    assert_eq!(kinds.iter().filter(|kind| *kind == "pi.compaction").count(), 1);
    assert_eq!(kinds.last().map(String::as_str), Some("pi.assistant"));
    chat.close().await;
}

#[tokio::test]
async fn fails_without_compacting_when_compaction_is_disabled_even_for_a_retryable_looking_overflow() {
    let chat = open(OpenOptions::default()).await;
    chat.history().await;
    chat.faux.agent(failure(&format!("overloaded: {OVERFLOW}")));
    let input = chat.submit(&text("u4", 100)).await;
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    // Three history turns, then the failing request: nothing is retried and nothing summarized.
    assert_eq!(chat.faux.agent_requests().len(), 4);
    assert!(chat.faux.summary_requests().is_empty());
    chat.close().await;
}

#[tokio::test]
async fn fails_after_a_blocking_threshold_compaction_in_the_same_generation() {
    let chat = open(window(1000)).await;
    chat.history().await;
    chat.set_policy(blocking());
    chat.faux.summary(summary());
    chat.faux.agent(failure(OVERFLOW));
    let input = chat.submit(&text("u4", 200)).await;
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(chat.faux.summary_requests().len(), 1);
    chat.close().await;
}

async fn fails_with_the_overflow_text(prepare: impl FnOnce(&Chat), requests: usize) {
    let chat = open(OpenOptions::default()).await;
    chat.turn(&text("u1", 100), &text("a1", 100)).await;
    chat.set_policy(enabled());
    prepare(&chat);
    chat.faux.agent(failure(OVERFLOW));
    let input = chat.submit(&text("u4", 100)).await;
    let settled = input.wait(&ctx()).await.unwrap();
    assert_eq!((settled.status, settled.reason.as_deref()), (SubmissionStatus::Unanswered, Some("model_error")));
    assert_eq!(settled.detail.map(|detail| detail.to_serde()), Some(json!(OVERFLOW)));
    assert_eq!(chat.faux.summary_requests().len(), requests);
    chat.close().await;
}

#[tokio::test]
async fn fails_with_the_overflow_text_when_compaction_declines() {
    let decline = |chat: &Chat| before_compact(chat, "declines", |_| Ok(Some(CompactionDecision::Decline)));
    fails_with_the_overflow_text(decline, 0).await;
}

#[tokio::test]
async fn fails_with_the_overflow_text_when_compaction_fails() {
    fails_with_the_overflow_text(|chat| chat.faux.summary(failure("bad request")), 1).await;
}

#[tokio::test]
async fn fails_with_the_overflow_text_when_compaction_cannot_cut() {
    // Classification finds no cut, so no compaction starts and the ordinary failure carries the text.
    let no_cut = |chat: &Chat| chat.set_policy(CompactionOverrides { keep_recent_tokens: Some(100_000), ..enabled() });
    fails_with_the_overflow_text(no_cut, 0).await;
}
