//! `!cmd` and `!!cmd` execution (upstream `agent-session-bash-persistence` tests).

use std::time::Duration;

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use serde_json::json;

use crate::harness::{Options, harness, text, wait_until};

#[tokio::test]
async fn records_bash_results_and_sends_them_to_the_model() {
    let h = harness(Options {
        responses: vec![FauxResponse::text("seen")],
        settings: json!({"shellCommandPrefix": "export GREETING=hello"}),
        ..Options::default()
    })
    .await;
    let mut chunks = String::new();
    let result = h
        .session
        .execute_bash("echo $GREETING; pwd", false, Some("b1"), &mut |chunk| chunks.push_str(chunk))
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(0));
    assert!(result.output.starts_with("hello\n") && chunks.contains("hello"));
    let updates = h.events_of("bash_execution_update");
    assert!(!updates.is_empty() && updates[0]["id"] == "b1");

    let hidden = h.session.execute_bash("echo secret", true, None, &mut |_| {}).await.unwrap();
    assert_eq!(hidden.output, "secret\n");
    let stored: Vec<_> = h.entries().into_iter().filter(|entry| entry["message"]["role"] == "bashExecution").collect();
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1]["message"]["excludeFromContext"], true);
    assert!(h.events_of("message_end").is_empty(), "bash messages emit no message events");

    h.session.prompt("what did it print?", Vec::new()).await.unwrap();
    let request = h.requests().pop().unwrap();
    let sent: Vec<String> = request.messages.iter().map(text).collect();
    assert!(sent.iter().any(|message| message.contains("echo $GREETING") && message.contains("hello")));
    assert!(!sent.iter().any(|message| message.contains("secret")));
}

#[tokio::test]
async fn defers_results_while_streaming_and_cancels_commands() {
    let h = harness(Options {
        responses: vec![FauxResponse::text("x".repeat(200)), FauxResponse::text("next")],
        delay: Some(Duration::from_millis(2)),
        ..Options::default()
    })
    .await;
    let session = h.session.clone();
    let running = tokio::spawn(async move { session.prompt("long", Vec::new()).await });
    wait_until(|| h.session.is_streaming()).await;
    h.session.execute_bash("echo during", false, None, &mut |_| {}).await.unwrap();
    assert!(h.session.has_pending_bash_messages());
    running.await.unwrap().unwrap();
    assert!(!h.session.has_pending_bash_messages());
    let messages = h.session.messages();
    assert!(matches!(messages.last(), Some(Message::Custom(custom)) if custom.role == "bashExecution"));

    let session = h.session.clone();
    let sleeping = tokio::spawn(async move { session.execute_bash("sleep 5", false, None, &mut |_| {}).await });
    wait_until(|| h.session.is_bash_running()).await;
    h.session.abort_bash();
    let result = tokio::time::timeout(Duration::from_secs(3), sleeping).await.unwrap().unwrap().unwrap();
    assert!(result.cancelled);
    assert!(!h.session.is_bash_running());
}
