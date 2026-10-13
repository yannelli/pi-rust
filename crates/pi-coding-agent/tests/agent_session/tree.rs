//! Tree navigation, names, labels, stats, and export (upstream `agent-session-tree-navigation`
//! and `agent-session-stats` tests).

use pi_ai::Message;
use pi_ai::providers::faux::FauxResponse;
use pi_coding_agent::agent_session::NavigateTreeOptions;
use serde_json::{Value, json};

use crate::harness::{Harness, Options, harness, roles};

/// Session entry ids of user messages, in order.
fn user_entry_ids(h: &Harness) -> Vec<String> {
    h.entries()
        .iter()
        .filter(|entry| entry["type"] == "message" && entry["message"]["role"] == "user")
        .map(|entry| entry["id"].as_str().unwrap().to_string())
        .collect()
}

async fn two_turns(extra: Vec<FauxResponse>) -> Harness {
    let mut responses = vec![FauxResponse::text("first answer"), FauxResponse::text("second answer")];
    responses.extend(extra);
    let h = harness(Options { responses, ..Options::default() }).await;
    h.session.prompt("first question", Vec::new()).await.unwrap();
    h.session.prompt("second question", Vec::new()).await.unwrap();
    h
}

#[tokio::test]
async fn navigates_to_a_user_message_and_returns_its_text() {
    let h = two_turns(Vec::new()).await;
    let second = user_entry_ids(&h)[1].clone();
    let result = h.session.navigate_tree(&second, NavigateTreeOptions::default()).await.unwrap();
    assert_eq!(result.editor_text.as_deref(), Some("second question"));
    assert!(!result.cancelled && result.summary_entry_id.is_none());
    assert_eq!(roles(&h.session.messages()), ["system", "user", "assistant"]);
    assert_eq!(h.requests().len(), 2, "no summary without summarize");

    let same = h.session.session_manager().leaf_id().unwrap().to_string();
    let noop = h.session.navigate_tree(&same, NavigateTreeOptions::default()).await.unwrap();
    assert_eq!(noop.editor_text, None);
    let missing = h.session.navigate_tree("nope", NavigateTreeOptions::default()).await.unwrap_err();
    assert_eq!(missing.to_string(), "Entry nope not found");
}

#[tokio::test]
async fn summarizes_the_abandoned_branch_with_custom_instructions() {
    let h = two_turns(vec![FauxResponse::text("## Branch\nWe discussed the second question.")]).await;
    let second = user_entry_ids(&h)[1].clone();
    let options = NavigateTreeOptions {
        summarize: true,
        custom_instructions: Some("Mention the answers".into()),
        label: Some("checkpoint".into()),
        ..NavigateTreeOptions::default()
    };
    let result = h.session.navigate_tree(&second, options).await.unwrap();
    let summary_id = result.summary_entry_id.clone().unwrap();
    let request = h.requests().pop().unwrap();
    let prompt = crate::harness::text(request.messages.last().unwrap());
    assert!(prompt.contains("Mention the answers"), "{prompt}");

    let entries = h.entries();
    let summary = entries.iter().find(|entry| entry["id"] == summary_id.as_str()).unwrap();
    assert_eq!(summary["type"], "branch_summary");
    assert!(summary["summary"].as_str().unwrap().contains("second question"));
    let labels: Vec<&Value> = entries.iter().filter(|entry| entry["type"] == "label").collect();
    assert_eq!(labels.len(), 1);
    assert_eq!(labels[0]["targetId"], summary_id.as_str());
    let messages = h.session.messages();
    assert!(matches!(messages.last(), Some(Message::Custom(custom)) if custom.role == "branchSummary"));
}

#[tokio::test]
async fn names_labels_and_reports_stats() {
    let h = two_turns(Vec::new()).await;
    h.session.set_session_name("  Release prep ").unwrap();
    assert_eq!(h.session.session_name().as_deref(), Some("Release prep"));
    assert_eq!(h.events_of("session_info_changed"), [json!({"type": "session_info_changed", "name": "Release prep"})]);
    let first = user_entry_ids(&h)[0].clone();
    h.session.set_label(&first, Some("start")).unwrap();
    assert!(h.entries().iter().any(|entry| entry["type"] == "label" && entry["label"] == "start"));

    let report = h.session.session_stats().to_json();
    assert_eq!((report["userMessages"].clone(), report["assistantMessages"].clone()), (json!(2), json!(2)));
    assert!(report["tokens"]["input"].as_u64().unwrap() > 0);
    assert_eq!(report["sessionId"], h.session.session_id());
    let usage = h.session.context_usage().unwrap();
    assert_eq!(usage.context_window, 128_000);
    assert!(usage.tokens.unwrap() > 0);

    let path = h.session.export_to_html(Some(h.cwd.join("out.html").to_str().unwrap())).unwrap();
    assert_eq!(path, h.cwd.join("out.html"));
    assert!(std::fs::read_to_string(path).unwrap().contains("<html"));
    let forks = h.session.user_messages_for_forking();
    assert_eq!(forks.iter().map(|(_, text)| text.as_str()).collect::<Vec<_>>(), ["first question", "second question"]);
}

#[tokio::test]
async fn in_memory_sessions_cannot_export_html() {
    let h = harness(Options { persist: false, ..Options::default() }).await;
    let error = h.session.export_to_html(None).unwrap_err();
    assert_eq!(error.to_string(), "Cannot export in-memory session to HTML");
}
