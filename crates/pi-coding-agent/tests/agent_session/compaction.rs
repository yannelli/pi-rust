//! Manual, threshold, between-turn, and overflow compaction (upstream `agent-session-compaction`
//! and `agent-session-auto-compaction-queue` tests).

use pi_ai::providers::faux::FauxResponse;
use pi_ai::{
    AssistantMessage, ContentBlock, Message, Model, StopReason, ToolCall, ToolResultMessage, UserContent, UserMessage,
};
use pi_coding_agent::session::SessionManager;
use serde_json::{Value, json};

use crate::harness::{Options, harness, open_session, record, roles, text};

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
    let long = "x".repeat(6500);
    let h = harness(Options {
        responses: vec![
            FauxResponse::text(format!("first answer {long}")),
            FauxResponse::text(format!("second answer {long}")),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("third answer"),
        ],
        context_window: 8000,
        settings: json!({"compaction": {"reserveTokens": 2500, "keepRecentTokens": 4000}}),
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

const COPILOT_413: &str = r#"github-copilot API error (413): {"message":"failed to parse request","code":""}"#;
const REQUEST_LIMIT: usize = 32 * 1024 * 1024;

fn user(content: String, timestamp: i64) -> Message {
    Message::User(UserMessage { content: UserContent::Text(content), timestamp })
}

fn assistant(model: &Model, content: Vec<ContentBlock>, stop: StopReason, timestamp: i64) -> Message {
    Message::Assistant(AssistantMessage { content, stop_reason: stop, timestamp, ..AssistantMessage::empty(model) })
}

/// A finished image-heavy turn, then a recent text turn that fits the retained tail.
fn seed_image_history(manager: &mut SessionManager, model: &Model, image: &str) {
    let timestamp = pi_ai::now_ms() - 10_000;
    let call = ToolCall { id: "read-images".into(), name: "read".into(), ..ToolCall::default() };
    let images = (0..89).map(|_| ContentBlock::Image { data: image.into(), mime_type: "image/png".into() }).collect();
    let history = [
        user("Review the old images".into(), timestamp),
        assistant(model, vec![ContentBlock::ToolCall(call)], StopReason::ToolUse, timestamp),
        Message::ToolResult(ToolResultMessage {
            tool_call_id: "read-images".into(),
            tool_name: "read".into(),
            content: images,
            timestamp,
            ..ToolResultMessage::default()
        }),
        assistant(model, vec![ContentBlock::text("Finished reviewing the old images")], StopReason::Stop, timestamp),
        user(format!("Recent task notes: {}", "x".repeat(80_000)), timestamp),
        assistant(model, vec![ContentBlock::text("Ready to continue")], StopReason::Stop, timestamp),
    ];
    for message in &history {
        manager.append_message(message).unwrap();
    }
}

fn has_image(message: &Message) -> bool {
    let blocks = match message {
        Message::User(UserMessage { content: UserContent::Blocks(blocks), .. }) => blocks,
        Message::ToolResult(result) => &result.content,
        _ => return false,
    };
    blocks.iter().any(|block| matches!(block, ContentBlock::Image { .. }))
}

#[tokio::test]
async fn compacts_an_image_heavy_history_and_retries_after_a_413() {
    let h = harness(Options {
        responses: vec![
            // The zero-usage error a Copilot request-size rejection leaves.
            FauxResponse::Error(COPILOT_413.into()),
            FauxResponse::text(SUMMARY),
            FauxResponse::text("Continued after compaction"),
        ],
        context_window: 1_000_000,
        ..Options::default()
    })
    .await;
    let model = h.session.model().unwrap();
    let mut manager = SessionManager::in_memory(&h.cwd);
    // Synthetic base64 only; nothing decodes an image or reaches a network.
    seed_image_history(&mut manager, &model, &"A".repeat(384 * 1024));
    let session = open_session(&h.services, manager).await;
    let events = record(&session);

    session.prompt("Continue the task using the recent notes.", Vec::new()).await.unwrap();

    let requests = h.requests();
    assert_eq!(requests.len(), 3, "the 413, the summary, then the retry");
    assert!(serde_json::to_vec(&requests[0]).unwrap().len() > REQUEST_LIMIT);
    assert!(is_summary_request(&requests[1]));
    let retried = &requests[2].messages;
    assert!(text(&retried[0]).contains("Summarized"));
    assert!(!retried.iter().any(has_image), "the retry no longer carries the images");
    assert!(serde_json::to_vec(&requests[2]).unwrap().len() < REQUEST_LIMIT);

    let events = compaction_events(&events.lock().unwrap());
    assert_eq!(events[0], json!({"type": "compaction_start", "reason": "overflow"}));
    assert_eq!(events[1]["reason"], "overflow");
    assert_eq!((events[1]["aborted"].clone(), events[1]["willRetry"].clone()), (json!(false), json!(true)));
    assert_eq!(session.last_assistant_text().as_deref(), Some("Continued after compaction"));
}
