//! Auto retry (upstream `agent-session-retry` and `agent-session-retry-events` tests).

use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::session::SessionManager;
use serde_json::{Value, json};

use crate::harness::{Options, harness, open_session, roles, wait_until};

fn retry_events(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .filter(|event| {
            let kind = event["type"].as_str().unwrap_or_default();
            kind.starts_with("auto_retry") || kind == "agent_end"
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn retries_transient_errors_up_to_max_retries() {
    let h = harness(Options {
        responses: (0..3).map(|_| FauxResponse::Error("503 service unavailable".into())).collect(),
        settings: json!({"retry": {"baseDelayMs": 1, "maxRetries": 2}}),
        ..Options::default()
    })
    .await;
    h.session.prompt("hi", Vec::new()).await.unwrap();

    assert_eq!(h.requests().len(), 3);
    let events = retry_events(&h.events());
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["agent_end", "auto_retry_start", "agent_end", "auto_retry_start", "agent_end", "auto_retry_end"]
    );
    assert_eq!((events[1]["delayMs"].as_u64(), events[3]["delayMs"].as_u64()), (Some(1), Some(2)));
    assert_eq!((events[1]["attempt"].as_u64(), events[1]["maxAttempts"].as_u64()), (Some(1), Some(2)));
    assert_eq!(events[4]["willRetry"], false);
    assert_eq!(events[5]["success"], false);
    assert_eq!(events[5]["attempt"], 2);
    assert!(events[5]["finalError"].as_str().unwrap().contains("503"));
    assert_eq!(roles(&h.session.messages()), ["system", "user", "assistant"], "earlier failures leave the context");
}

#[tokio::test]
async fn hides_retried_failures_on_resume_and_reports_success() {
    let h = harness(Options {
        responses: vec![FauxResponse::Error("overloaded_error".into()), FauxResponse::text("recovered")],
        settings: json!({"retry": {"baseDelayMs": 1}}),
        ..Options::default()
    })
    .await;
    h.session.prompt("hi", Vec::new()).await.unwrap();
    let ends = h.events_of("auto_retry_end");
    assert_eq!(ends, [json!({"type": "auto_retry_end", "success": true, "attempt": 1})]);
    assert_eq!(h.requests().len(), 2);
    assert_eq!(h.requests()[1].messages.len(), 1, "the failed response is not sent again");

    let edits: Vec<Value> = h.entries().into_iter().filter(|entry| entry["type"] == "context_edit").collect();
    assert_eq!(edits.len(), 1);
    let resumed = open_session(&h.services, SessionManager::open(&h.session.session_file().unwrap()).unwrap()).await;
    assert_eq!(roles(&resumed.messages()), ["system", "user", "assistant"]);
}

#[tokio::test]
async fn abort_cancels_a_pending_retry() {
    let h = harness(Options {
        responses: vec![FauxResponse::Error("overloaded".into())],
        settings: json!({"retry": {"baseDelayMs": 60_000}}),
        persist: false,
        ..Options::default()
    })
    .await;
    let session = h.session.clone();
    let running = tokio::spawn(async move { session.prompt("hi", Vec::new()).await });
    wait_until(|| h.session.is_retrying()).await;
    h.session.abort().await;
    running.await.unwrap().unwrap();
    let events = retry_events(&h.events());
    assert_eq!(
        events.last().unwrap(),
        &json!({"type": "auto_retry_end", "success": false, "attempt": 1, "finalError": "Retry cancelled"})
    );
    assert_eq!(h.requests().len(), 1);
    assert_eq!(h.events().last().unwrap()["type"], "agent_settled");
}

#[tokio::test]
async fn does_not_retry_when_disabled_or_not_retryable() {
    let h = harness(Options {
        responses: vec![FauxResponse::Error("503 service unavailable".into()), FauxResponse::Error("invalid x".into())],
        settings: json!({"retry": {"enabled": false}}),
        ..Options::default()
    })
    .await;
    h.session.prompt("one", Vec::new()).await.unwrap();
    h.session.set_auto_retry_enabled(true);
    h.session.prompt("two", Vec::new()).await.unwrap();
    assert_eq!(h.requests().len(), 2);
    assert!(h.events_of("auto_retry_start").is_empty());
}
