//! Prompting, queues, abort, and event streaming (upstream
//! `rpc-prompt-response-semantics.test.ts`).

use std::time::Duration;

use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, event_order};
use crate::support::start;

const PIXEL: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

#[tokio::test]
async fn prompt_reports_started_and_streams_events_until_settled() {
    let mut rpc = start(Options { responses: vec![FauxResponse::text("Hello there")], ..Options::default() }).await;
    let image = json!({"type": "image", "data": PIXEL, "mimeType": "image/png"});
    rpc.send(json!({"id": "p1", "type": "prompt", "message": "Hi", "images": [image]})).await;
    let response = rpc.response("p1").await;
    assert_eq!(
        response,
        json!({"id": "p1", "type": "response", "command": "prompt", "success": true, "data": {"disposition": "started"}})
    );
    let settled = rpc.settled(1).await;
    assert_eq!(settled, json!({"type": "agent_settled", "aborted": false}));

    let events: Vec<_> = rpc.records().into_iter().filter(|record| record["type"] != "response").collect();
    let order = event_order(&events);
    assert_eq!(order.first().map(String::as_str), Some("agent_start"));
    assert!(order.contains(&"message_end:assistant".to_string()), "{order:?}");
    assert_eq!(&order[order.len() - 2..], ["agent_end", "agent_settled"]);
    let update = events.iter().find(|event| event["type"] == "message_update").unwrap();
    assert!(update.get("message").is_none(), "updates carry deltas only");
    assert_eq!(update["assistantMessageEvent"]["type"], "text_start");
    let deltas: String = events
        .iter()
        .filter(|event| event["assistantMessageEvent"]["type"] == "text_delta")
        .map(|event| event["assistantMessageEvent"]["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(deltas, "Hello there");
    let request = rpc.h.requests().pop().unwrap();
    let user = serde_json::to_value(request.messages.last().unwrap()).unwrap();
    assert_eq!(user["content"][1]["type"], "image", "the image reaches the model");

    let messages = rpc.data(json!({"id": "m", "type": "get_messages"})).await["messages"].clone();
    let roles: Vec<&str> = messages.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant"]);
    let text = rpc.data(json!({"id": "t", "type": "get_last_assistant_text"})).await;
    assert_eq!(text, json!({"text": "Hello there"}));
    let state = rpc.data(json!({"id": "s", "type": "get_state"})).await;
    assert_eq!((state["messageCount"].clone(), state["isStreaming"].clone()), (json!(3), json!(false)));
    assert_eq!(rpc.finish().await, 0);
}

#[tokio::test]
async fn rejected_prompts_get_exactly_one_error_response() {
    let mut rpc = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
    rpc.send(json!({"id": "start", "type": "prompt", "message": "Start"})).await;
    assert_eq!(rpc.response("start").await["data"]["disposition"], "started");
    rpc.wait_for(|record| record["type"] == "agent_start").await;
    rpc.send(json!({"id": "again", "type": "prompt", "message": "Again"})).await;
    let response = rpc.response("again").await;
    assert_eq!(response["success"], false);
    assert!(response["error"].as_str().unwrap().starts_with("Agent is already processing."), "{response}");
    rpc.send(json!({"id": "a", "type": "abort"})).await;
    assert_eq!(rpc.response("a").await, json!({"id": "a", "type": "response", "command": "abort", "success": true}));
    assert_eq!(rpc.settled(1).await["aborted"], true);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rpc.responses("again").len(), 1);
    assert_eq!(rpc.responses("start").len(), 1);
    rpc.finish().await;
}

#[tokio::test]
async fn prompt_without_auth_fails_before_acceptance() {
    let options = Options { responses: vec![FauxResponse::text("unused")], ..Options::default() };
    let mut rpc = crate::support::start_with(options, Default::default(), |h, app| {
        let path = h.agent_dir.join("models.json");
        let mut models: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        models["providers"]["keyless"] = json!({"baseUrl": "http://127.0.0.1:9", "api": "openai-completions",
            "models": [{"id": "free", "contextWindow": 1000, "maxTokens": 100}]});
        std::fs::write(&path, models.to_string()).unwrap();
        app.provider = Some("keyless".into());
        app.model = Some("free".into());
    })
    .await;
    rpc.send(json!({"id": "b1", "type": "prompt", "message": "Hello"})).await;
    let response = rpc.response("b1").await;
    assert_eq!(response["success"], false, "{response}");
    let error = response["error"].as_str().unwrap();
    assert!(error.starts_with("No API key found for keyless."), "{error}");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(rpc.responses("b1").len(), 1);
    assert!(rpc.records_of("agent_start").is_empty());
    rpc.finish().await;
}

#[tokio::test]
async fn queues_steering_and_follow_ups_then_clears_them() {
    let mut rpc = start(Options { responses: vec![FauxResponse::Hang], ..Options::default() }).await;
    rpc.send(json!({"id": "start", "type": "prompt", "message": "Start"})).await;
    rpc.response("start").await;
    let steer = json!({"id": "s1", "type": "prompt", "message": "Change direction", "streamingBehavior": "steer"});
    assert_eq!(rpc.data(steer).await, json!({"disposition": "queued"}));
    let follow =
        json!({"id": "f1", "type": "prompt", "message": "Summarize when finished", "streamingBehavior": "followUp"});
    assert_eq!(rpc.data(follow).await, json!({"disposition": "queued"}));
    assert_eq!(rpc.data(json!({"id": "s2", "type": "steer", "message": "Also this"})).await["disposition"], "queued");
    assert_eq!(
        rpc.data(json!({"id": "f2", "type": "follow_up", "message": "And that"})).await["disposition"],
        "queued"
    );
    let update = rpc
        .wait_for(|record| record["type"] == "queue_update" && record["followUp"].as_array().unwrap().len() == 2)
        .await;
    assert_eq!(
        update,
        json!({"type": "queue_update", "steering": ["Change direction", "Also this"], "followUp": ["Summarize when finished", "And that"]})
    );
    let state = rpc.data(json!({"id": "st", "type": "get_state"})).await;
    assert_eq!((state["pendingMessageCount"].clone(), state["isStreaming"].clone()), (json!(4), json!(true)));

    let cleared = rpc.data(json!({"id": "clear", "type": "clear_queue"})).await;
    assert_eq!(
        cleared,
        json!({"steering": ["Change direction", "Also this"], "followUp": ["Summarize when finished", "And that"]})
    );
    assert_eq!(rpc.data(json!({"id": "a", "type": "abort"})).await, serde_json::Value::Null);
    assert_eq!(rpc.settled(1).await["aborted"], true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rpc.records_of("agent_start").len(), 1, "cleared messages never run");
    assert_eq!(rpc.data(json!({"id": "st2", "type": "get_state"})).await["pendingMessageCount"], 0);
    rpc.finish().await;
}

#[tokio::test]
async fn follow_ups_run_after_the_current_turn() {
    let responses = vec![FauxResponse::text("first"), FauxResponse::text("second")];
    let mut rpc = start(Options { responses, delay: Some(Duration::from_millis(30)), ..Options::default() }).await;
    rpc.send(json!({"id": "p", "type": "prompt", "message": "One"})).await;
    rpc.response("p").await;
    let queued =
        rpc.data(json!({"id": "q", "type": "prompt", "message": "Two", "streamingBehavior": "followUp"})).await;
    assert_eq!(queued["disposition"], "queued");
    rpc.settled(1).await;
    let text = rpc.data(json!({"id": "t", "type": "get_last_assistant_text"})).await;
    assert_eq!(text["text"], "second");
    let users: Vec<String> = rpc
        .records_of("message_end")
        .into_iter()
        .filter(|event| event["message"]["role"] == "user")
        .map(|event| event["message"]["content"][0]["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(users, ["One", "Two"]);
    rpc.finish().await;
}

#[tokio::test]
async fn retries_report_auto_retry_events_and_abort_retry_stops_them() {
    let settings = json!({"retry": {"enabled": true, "maxRetries": 3, "baseDelayMs": 5000}});
    let responses = vec![FauxResponse::Error("503 Service Unavailable: overloaded".into())];
    let mut rpc = start(Options { responses, settings, ..Options::default() }).await;
    rpc.send(json!({"id": "p", "type": "prompt", "message": "Try"})).await;
    let start = rpc.wait_for(|record| record["type"] == "auto_retry_start").await;
    assert_eq!((start["attempt"].clone(), start["maxAttempts"].clone()), (json!(1), json!(3)));
    assert_eq!(rpc.data(json!({"id": "r", "type": "abort_retry"})).await, serde_json::Value::Null);
    let end = rpc.wait_for(|record| record["type"] == "auto_retry_end").await;
    assert_eq!(end, json!({"type": "auto_retry_end", "success": false, "attempt": 1, "finalError": "Retry cancelled"}));
    rpc.settled(1).await;
    assert_eq!(
        rpc.data(json!({"id": "off", "type": "set_auto_retry", "enabled": false})).await,
        serde_json::Value::Null
    );
    assert!(!rpc.h.settings.retry_enabled());
    rpc.finish().await;
}
