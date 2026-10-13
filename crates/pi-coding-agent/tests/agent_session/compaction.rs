//! Manual, threshold, between-turn, and overflow compaction (upstream `agent-session-compaction`
//! and `agent-session-auto-compaction-queue` tests).

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{ContentBlock, Message, StopReason};
use serde_json::{Value, json};

use crate::harness::{Options, harness, roles, text};

const SUMMARY: &str = "## Goal\nSummarized";

fn compaction_events(events: &[Value]) -> Vec<Value> {
    let kinds = ["compaction_start", "compaction_end"];
    events.iter().filter(|event| kinds.contains(&event["type"].as_str().unwrap_or_default())).cloned().collect()
}

/// A user turn long enough that compaction keeps it whole.
fn second() -> String {
    format!("second {}", "s".repeat(400))
}

fn is_summary_request(context: &pi_ai::Context) -> bool {
    context
        .system_prompt
        .as_deref()
        .is_some_and(|prompt| prompt.starts_with("You are a context summarization assistant"))
}

#[tokio::test]
async fn compacts_manually_and_persists_the_entry() {
    let h = harness(Options {
        responses: vec![
            FauxResponse::text("first answer"),
            FauxResponse::text("second answer"),
            FauxResponse::text(SUMMARY),
        ],
        settings: json!({"compaction": {"keepRecentTokens": 6}}),
        ..Options::default()
    })
    .await;
    h.session.prompt("first question", Vec::new()).await.unwrap();
    h.session.prompt("second question", Vec::new()).await.unwrap();
    let result = h.session.compact(Some("focus on answers")).await.unwrap();
    assert_eq!(result.summary, SUMMARY);
    let summary_request = h.requests().pop().unwrap();
    assert!(is_summary_request(&summary_request));
    assert!(text(summary_request.messages.last().unwrap()).contains("focus on answers"));

    let events = compaction_events(&h.events());
    assert_eq!(events[0], json!({"type": "compaction_start", "reason": "manual"}));
    assert_eq!(events[1]["reason"], "manual");
    assert_eq!(events[1]["result"]["summary"], SUMMARY);
    assert_eq!((events[1]["aborted"].clone(), events[1]["willRetry"].clone()), (json!(false), json!(false)));
    assert!(h.entries().iter().any(|entry| entry["type"] == "compaction"));
    let messages = h.session.messages();
    assert!(
        messages.iter().any(|message| matches!(message, Message::Custom(custom) if custom.role == "compactionSummary"))
    );
    let error = h.session.compact(None).await.unwrap_err().to_string();
    assert!(error == "Already compacted" || error.starts_with("Nothing to compact"), "{error}");
}

#[tokio::test]
async fn compacts_after_crossing_the_threshold() {
    let long = "x".repeat(4000);
    let h = harness(Options {
        responses: vec![
            FauxResponse::text(format!("first answer {long}")),
            FauxResponse::text(format!("second answer {long}")),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("third answer"),
        ],
        context_window: 8000,
        settings: json!({"compaction": {"reserveTokens": 2500, "keepRecentTokens": 1500}}),
        ..Options::default()
    })
    .await;
    h.session.prompt(&format!("first question {long}"), Vec::new()).await.unwrap();
    h.session.prompt(&format!("second question {long}"), Vec::new()).await.unwrap();
    h.session.prompt("third question", Vec::new()).await.unwrap();

    let requests = h.requests();
    assert_eq!(requests.len(), 4, "compaction events: {:?}", compaction_events(&h.events()));
    assert!(is_summary_request(&requests[2]));
    let after = &requests[3].messages;
    assert!(text(&after[0]).contains("Summarized"));
    assert!(after.len() < 6, "older turns were replaced by the summary");
    let events = compaction_events(&h.events());
    assert_eq!(events[0], json!({"type": "compaction_start", "reason": "threshold"}));
    assert_eq!(events[1]["willRetry"], false);
}

#[tokio::test]
async fn compacts_between_turns_of_one_run() {
    let h = harness(Options {
        responses: vec![
            FauxResponse::text("noted"),
            FauxResponse::tool_call("r1", "read", json!({"path": "big.txt"})),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("Turn prefix"),
            FauxResponse::text("done"),
        ],
        context_window: 16_000,
        settings: json!({"compaction": {"reserveTokens": 2000, "keepRecentTokens": 1}}),
        ..Options::default()
    })
    .await;
    std::fs::write(h.cwd.join("big.txt"), format!("{}\n", "z".repeat(79)).repeat(300)).unwrap();
    h.session.prompt(&format!("remember {}", "y".repeat(28_000)), Vec::new()).await.unwrap();
    assert!(h.events_of("compaction_start").is_empty());
    h.session.prompt("read big.txt", Vec::new()).await.unwrap();

    let requests = h.requests();
    assert_eq!(requests.len(), 5, "history and split-turn summaries run before the next turn");
    assert!(is_summary_request(&requests[2]) && is_summary_request(&requests[3]));
    assert!(text(&requests[4].messages[0]).contains("Summarized"));
    assert!(matches!(requests[4].messages.last(), Some(Message::ToolResult(_))));
    let events = compaction_events(&h.events());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["reason"], "threshold");
    assert_eq!(h.session.last_assistant_text().as_deref(), Some("done"));
}

#[tokio::test]
async fn recovers_from_context_overflow_once() {
    let long = "w".repeat(8000);
    let overflow = "prompt is too long: 213462 tokens > 200000 maximum";
    let h = harness(Options {
        responses: vec![
            FauxResponse::text(format!("first {long}")),
            FauxResponse::Error(overflow.into()),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("Turn prefix"),
            FauxResponse::text("recovered"),
        ],
        settings: json!({"compaction": {"keepRecentTokens": 50}}),
        ..Options::default()
    })
    .await;
    h.session.prompt(&format!("first {long}"), Vec::new()).await.unwrap();
    h.session.prompt(&second(), Vec::new()).await.unwrap();

    let requests = h.requests();
    assert_eq!(requests.len(), 5, "history and turn-prefix summaries, then the retry");
    assert!(is_summary_request(&requests[2]) && is_summary_request(&requests[3]));
    let retried = &requests[4].messages;
    assert!(text(&retried[0]).contains("Summarized"));
    assert!(
        !retried.iter().any(|message| matches!(message, Message::Assistant(a) if a.stop_reason == StopReason::Error))
    );
    let events = compaction_events(&h.events());
    assert_eq!(events[0], json!({"type": "compaction_start", "reason": "overflow"}));
    assert_eq!((events[1]["reason"].clone(), events[1]["willRetry"].clone()), (json!("overflow"), json!(true)));
    assert_eq!(h.session.last_assistant_text().as_deref(), Some("recovered"));
    assert_eq!(roles(&h.session.messages()).last(), Some(&"assistant"));
}

#[tokio::test]
async fn compacts_and_resumes_after_a_short_length_stop() {
    let long = "v".repeat(8000);
    let truncated = FauxResponse::Message { content: vec![ContentBlock::text("partial")], stop: StopReason::Length };
    let h = harness(Options {
        responses: vec![
            FauxResponse::text(format!("first {long}")),
            truncated.clone(),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("Turn prefix"),
            truncated,
        ],
        settings: json!({"compaction": {"keepRecentTokens": 50}}),
        ..Options::default()
    })
    .await;
    h.session.prompt(&format!("first {long}"), Vec::new()).await.unwrap();
    h.session.prompt(&second(), Vec::new()).await.unwrap();

    assert_eq!(h.requests().len(), 5, "one compact-and-retry, then the second truncation stands");
    let events = compaction_events(&h.events());
    assert_eq!(events[0]["reason"], "overflow");
    assert_eq!(events[1]["willRetry"], true);
    let Some(Message::Assistant(last)) = h.session.messages().pop() else { panic!("assistant") };
    assert_eq!(last.stop_reason, StopReason::Length);
}
