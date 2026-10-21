//! Upstream `session-events` (turns, queue, cancel, usage), `session-queue-cancel`,
//! `session-auto-title`, `session-slash-commands`, and `context-usage` session tests.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::{FakeBackend, FakeClient, session_with, settle};
use crate::acp::session::{AcpSession, AcpSessionParams, TurnEnd, TurnResult};

fn here() -> std::path::PathBuf {
    std::env::current_dir().unwrap()
}

fn start(session: &Arc<AcpSession>, text: &str, title: Option<&str>) -> JoinHandle<TurnResult> {
    let (session, text, title) = (session.clone(), text.to_string(), title.map(String::from));
    tokio::spawn(async move { session.prompt(text, Vec::new(), title).await })
}

async fn finish_run(session: &AcpSession) {
    for kind in ["agent_start", "turn_end", "agent_end", "agent_settled"] {
        session.handle_pi_event(json!({"type": kind})).await;
    }
}

fn titled(session: &Arc<FakeBackend>, client: &Arc<FakeClient>, auto_title: bool) -> Arc<AcpSession> {
    AcpSession::new(AcpSessionParams {
        session_id: "s1".into(),
        cwd: here(),
        backend: session.clone(),
        client: client.clone(),
        auto_title,
    })
}

fn titles(client: &FakeClient) -> Vec<Value> {
    client.update_bodies().into_iter().filter_map(|update| update.get("title").cloned()).collect()
}

#[tokio::test]
async fn prompts_stay_open_through_retries_until_agent_settled() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    let turn = start(&session, "hello", None);
    settle().await;
    assert_eq!(backend.prompt_texts(), ["hello"]);
    session.handle_pi_event(json!({"type": "agent_start"})).await;
    session.handle_pi_event(json!({"type": "auto_retry_start", "attempt": 1, "maxAttempts": 3, "delayMs": 2000})).await;
    session.handle_pi_event(json!({"type": "agent_end", "willRetry": true})).await;
    session.handle_pi_event(json!({"type": "agent_start"})).await;
    session.handle_pi_event(json!({"type": "agent_end", "willRetry": false})).await;
    settle().await;
    assert!(!turn.is_finished());
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
}

#[tokio::test]
async fn startup_info_is_sent_once() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    let notice = "New version available: v0.74.0 (installed v0.73.1).";
    session.set_startup_info(notice.into());
    session.send_startup_info_if_pending();
    session.send_startup_info_if_pending();
    let turn = start(&session, "hello", None);
    settle().await;
    let count = client.update_bodies().iter().filter(|update| update["content"]["text"] == notice).count();
    assert_eq!(count, 1);
    finish_run(&session).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
}

#[tokio::test]
async fn cancel_turns_the_stop_reason_into_cancelled() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().stats = Box::new(|_| Ok(json!({"contextUsage": {"tokens": 42, "contextWindow": 100}})));
    let session = session_with(&backend, &client, &here());
    let turn = start(&session, "hello", None);
    settle().await;
    session.cancel().await;
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::Cancelled);
    assert_eq!(backend.aborts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(client.of_kind("usage_update"), [json!({"sessionUpdate": "usage_update", "used": 42, "size": 100})]);
}

#[tokio::test]
async fn concurrent_prompts_queue_until_the_turn_settles() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    let first = start(&session, "one", None);
    settle().await;
    let second = start(&session, "two", None);
    settle().await;
    assert_eq!(backend.prompt_texts(), ["one"]);
    assert!(client.update_bodies().iter().any(|update| update["content"]["text"] == "Queued message (position 1)."));
    assert!(
        client
            .update_bodies()
            .iter()
            .any(|update| update["_meta"]["piAcp"] == json!({"queueDepth": 1, "running": true}))
    );
    finish_run(&session).await;
    assert_eq!(first.await.unwrap().unwrap(), TurnEnd::EndTurn);
    settle().await;
    assert_eq!(backend.prompt_texts(), ["one", "two"]);
    assert!(
        client
            .update_bodies()
            .iter()
            .any(|update| update["content"]["text"] == "Starting queued message. (0 remaining)")
    );
    finish_run(&session).await;
    assert_eq!(second.await.unwrap().unwrap(), TurnEnd::EndTurn);
    let last = client.update_bodies().pop().unwrap();
    assert_eq!(last["_meta"]["piAcp"], json!({"queueDepth": 0, "running": false}));
}

#[tokio::test]
async fn cancel_clears_queued_prompts() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    let first = start(&session, "one", None);
    settle().await;
    let second = start(&session, "two", None);
    settle().await;
    session.cancel().await;
    assert_eq!(second.await.unwrap().unwrap(), TurnEnd::Cancelled);
    assert!(client.update_bodies().iter().any(|update| update["content"]["text"] == "Cleared queued prompts."));
    finish_run(&session).await;
    assert_eq!(first.await.unwrap().unwrap(), TurnEnd::Cancelled);
    assert_eq!(backend.prompt_texts(), ["one"]);
}

#[tokio::test]
async fn slash_commands_reach_pi_unchanged_for_its_own_expansion() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    let turn = start(&session, "/hello world", None);
    settle().await;
    assert_eq!(backend.prompt_texts(), ["/hello world"]);
    finish_run(&session).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
}

#[tokio::test]
async fn rejected_prompts_end_the_turn_or_require_auth() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = session_with(&backend, &client, &here());
    backend.config().prompt_error = Some("socket closed".into());
    assert_eq!(session.prompt("a".into(), Vec::new(), None).await.unwrap(), TurnEnd::Error);
    backend.config().prompt_error = Some("No API key found for anthropic".into());
    let error = session.prompt("b".into(), Vec::new(), None).await.unwrap_err();
    assert_eq!(crate::acp::errors::code(&error), -32000);
    let last = client.update_bodies().pop().unwrap();
    assert_eq!(last["_meta"]["piAcp"], json!({"queueDepth": 0, "running": false}));
}

#[tokio::test]
async fn usage_updates_follow_context_usage_and_skip_bad_values() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().stats = Box::new(|_| {
        Ok(json!({"tokens": {"total": 999_999}, "contextUsage": {"tokens": 12_345, "contextWindow": 200_000}}))
    });
    let session = session_with(&backend, &client, &here());
    let turn = start(&session, "hello", None);
    settle().await;
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
    assert_eq!(
        client.of_kind("usage_update"),
        [json!({"sessionUpdate": "usage_update", "used": 12_345, "size": 200_000})]
    );

    let invalid = [
        json!({}),
        json!({"contextUsage": {"tokens": null, "contextWindow": 200_000}}),
        json!({"contextUsage": {"tokens": -1, "contextWindow": 100}}),
        json!({"contextUsage": {"tokens": 1.5, "contextWindow": 100}}),
        json!({"contextUsage": {"tokens": 10, "contextWindow": 0}}),
        json!({"contextUsage": {"tokens": 10, "contextWindow": -1}}),
        json!({"contextUsage": {"tokens": 10, "contextWindow": 100.5}}),
        json!({"contextUsage": {"tokens": 10, "contextWindow": null}}),
        json!({"contextUsage": {"tokens": 10, "contextWindow": 1e300}}),
    ];
    for stats in invalid {
        assert!(crate::acp::to_usage_update(&stats).is_none(), "{stats}");
    }
}

#[tokio::test]
async fn usage_updates_carry_the_session_cost() {
    let stats = |cost: Value| json!({"cost": cost, "contextUsage": {"tokens": 10, "contextWindow": 100}});
    let update = |cost| serde_json::to_value(crate::acp::to_usage_update(&stats(cost)).unwrap()).unwrap();
    assert_eq!(update(json!(0.125)), json!({"used": 10, "size": 100, "cost": {"amount": 0.125, "currency": "USD"}}));
    assert_eq!(update(json!(-1)), json!({"used": 10, "size": 100}));
    assert_eq!(update(json!("free")), json!({"used": 10, "size": 100}));
}

#[tokio::test]
async fn the_prompt_response_reports_the_turns_token_usage() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().stats = Box::new(|_| Ok(json!({"contextUsage": {"tokens": 7, "contextWindow": 100}})));
    let session = session_with(&backend, &client, &here());
    let turn = {
        let session = session.clone();
        tokio::spawn(async move { session.prompt_with_usage("hello".into(), Vec::new(), None).await })
    };
    settle().await;
    let usage = |input, output, reasoning: Option<u64>| {
        let mut usage = json!({"input": input, "output": output, "cacheRead": 5, "cacheWrite": 0, "totalTokens": input + output + 5});
        if let Some(reasoning) = reasoning {
            usage["reasoning"] = json!(reasoning);
        }
        usage
    };
    let end = |role, usage| json!({"type": "message_end", "message": {"role": role, "usage": usage}});
    session.handle_pi_event(end("user", usage(1000, 1000, None))).await;
    session.handle_pi_event(end("assistant", usage(100, 20, Some(8)))).await;
    assert_eq!(client.of_kind("usage_update").len(), 1, "an assistant message publishes context usage");
    session.handle_pi_event(end("assistant", usage(300, 40, None))).await;
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    let (result, usage) = turn.await.unwrap();
    assert_eq!(result.unwrap(), TurnEnd::EndTurn);
    assert_eq!(
        serde_json::to_value(usage.unwrap()).unwrap(),
        json!({"totalTokens": 470, "inputTokens": 400, "outputTokens": 60, "thoughtTokens": 8, "cachedReadTokens": 10, "cachedWriteTokens": 0})
    );
    assert_eq!(client.of_kind("usage_update").len(), 3);

    let quiet = {
        let session = session.clone();
        tokio::spawn(async move { session.prompt_with_usage("quiet".into(), Vec::new(), None).await })
    };
    settle().await;
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    assert_eq!(quiet.await.unwrap().1, None, "a turn without assistant messages reports no usage");
}

#[tokio::test]
async fn failing_or_slow_stats_do_not_block_the_turn() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().stats = Box::new(|_| Err(anyhow::anyhow!("pi get_session_stats failed: unsupported")));
    let session = session_with(&backend, &client, &here());
    let turn = start(&session, "hello", None);
    settle().await;
    session.handle_pi_event(json!({"type": "agent_settled"})).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
    assert!(client.of_kind("usage_update").is_empty());

    backend.config().stats = Box::new(|_| Ok(json!({"contextUsage": {"tokens": 1, "contextWindow": 2}})));
    backend.config().stats_delay = Some(Duration::from_secs(30));
    tokio::time::pause();
    let turn = start(&session, "again", None);
    settle().await;
    let settled = {
        let session = session.clone();
        tokio::spawn(async move { session.handle_pi_event(json!({"type": "agent_settled"})).await })
    };
    tokio::time::advance(crate::acp::SESSION_STATS_TIMEOUT + Duration::from_millis(10)).await;
    settled.await.unwrap();
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
    assert!(client.of_kind("usage_update").is_empty());
}

#[tokio::test]
async fn new_sessions_are_named_after_their_first_prompt() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().state = json!({});
    let session = titled(&backend, &client, true);
    session.set_startup_info("Welcome to pi".into());
    session.send_startup_info_if_pending();
    let turn = start(&session, "hello", Some("Fix thread titles"));
    settle().await;
    assert_eq!(backend.calls(), ["name:Fix thread titles"]);
    let info: Vec<Value> =
        client.of_kind("session_info_update").into_iter().filter(|u| u.get("title").is_some()).collect();
    assert_eq!(info.len(), 1);
    assert_eq!(info[0]["title"], "Fix thread titles");
    assert!(info[0]["updatedAt"].is_string());
    finish_run(&session).await;
    turn.await.unwrap().unwrap();
    let second = start(&session, "second prompt", Some("Second title"));
    settle().await;
    finish_run(&session).await;
    second.await.unwrap().unwrap();
    assert_eq!(backend.calls(), ["name:Fix thread titles"]);
}

#[tokio::test]
async fn existing_and_manual_names_win_over_automatic_titles() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().state = json!({"sessionName": "Manual title"});
    let session = titled(&backend, &client, true);
    let turn = start(&session, "hello", Some("Automatic title"));
    settle().await;
    finish_run(&session).await;
    turn.await.unwrap().unwrap();
    assert!(backend.calls().is_empty());
    assert_eq!(titles(&client).last().unwrap(), "Manual title");

    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().state = json!({});
    let session = titled(&backend, &client, true);
    let turn = start(&session, "hello", Some("Automatic title"));
    settle().await;
    session.set_session_name("Manual title").await.unwrap();
    finish_run(&session).await;
    turn.await.unwrap().unwrap();
    assert_eq!(backend.calls(), ["name:Automatic title", "name:Manual title"]);
    assert_eq!(titles(&client).last().unwrap(), "Manual title");
}

#[tokio::test]
async fn auto_title_failures_do_not_fail_the_prompt() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    backend.config().state = json!({});
    backend.config().set_name = Box::new(|_, _| Err(anyhow::anyhow!("set_session_name failed")));
    let session = titled(&backend, &client, true);
    let turn = start(&session, "hello", Some("Automatic title"));
    settle().await;
    finish_run(&session).await;
    assert_eq!(turn.await.unwrap().unwrap(), TurnEnd::EndTurn);
    assert!(titles(&client).is_empty());
}

#[tokio::test]
async fn pi_name_changes_are_forwarded_once_and_clears_are_kept() {
    let (client, backend) = (FakeClient::new(), FakeBackend::new());
    let session = titled(&backend, &client, false);
    for event in [
        json!({"type": "session_info_changed", "name": "Manual title"}),
        json!({"type": "session_info_changed", "name": "Manual title"}),
        json!({"type": "session_info_changed", "name": 123}),
        json!({"type": "session_info_changed", "name": {"text": "invalid"}}),
        json!({"type": "session_info_changed", "name": "Manual title"}),
        json!({"type": "session_info_changed"}),
        json!({"type": "session_info_changed"}),
    ] {
        session.handle_pi_event(event).await;
    }
    assert_eq!(titles(&client), [json!("Manual title"), Value::Null]);
}
