use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::{Message, register_api_provider};
use pi_coding_agent::app::{CodingSession, SessionOptions};
use pi_coding_agent::settings::SettingsManager;
use serde_json::json;

fn options(root: &Path, api: &str) -> SessionOptions {
    let agent_dir = root.join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"local": {"baseUrl": "http://unused", "api": api, "apiKey": "test-key",
            "models": [{"id": "tiny", "contextWindow": 6000, "maxTokens": 1000, "reasoning": true}]}}})
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    std::fs::write(root.join("work").join("AGENTS.md"), "Project rule: be brief.").unwrap();
    SessionOptions {
        cwd: root.join("work"),
        agent_dir,
        model: Some("local/tiny".into()),
        persist: true,
        ..SessionOptions::default()
    }
}

#[tokio::test]
async fn runs_tools_persists_and_resumes_sessions() {
    let root = tempfile::tempdir().unwrap();
    let provider = FauxProvider::new([
        FauxResponse::tool_call("w1", "write", json!({"path": "notes.txt", "content": "meeting on Friday"})),
        FauxResponse::tool_call("r1", "read", json!({"path": "notes.txt"})),
        FauxResponse::text("The meeting is on Friday."),
        FauxResponse::text("Still Friday."),
    ]);
    register_api_provider("faux-session", provider.clone());
    let session = CodingSession::new(options(root.path(), "faux-session")).await.unwrap();
    session.prompt("Write notes.txt, then read it back.", Vec::new()).await.unwrap();

    assert_eq!(std::fs::read_to_string(root.path().join("work/notes.txt")).unwrap(), "meeting on Friday");
    let requests = provider.requests();
    let system = requests[0].system_prompt.clone().unwrap();
    assert!(system.contains("Project rule: be brief.") && system.contains("- read: "));
    assert_eq!(
        requests[0].tools.iter().map(|tool| tool.name.as_str()).collect::<Vec<_>>(),
        ["read", "bash", "edit", "write"]
    );
    let read_result = requests[2].messages.iter().rev().find_map(|message| match message {
        Message::ToolResult(result) => Some(result.content[0].as_text().unwrap_or_default().to_string()),
        _ => None,
    });
    assert!(read_result.unwrap().contains("meeting on Friday"));
    let file = session.session_file().unwrap();
    assert!(file.exists());

    let mut resumed_options = options(root.path(), "faux-session");
    resumed_options.continue_recent = true;
    resumed_options.model = None;
    let resumed = CodingSession::new(resumed_options).await.unwrap();
    assert_eq!(resumed.session_file().unwrap(), file);
    assert_eq!(resumed.model().id, "tiny");
    assert_eq!(resumed.agent.state().messages.len(), 6);
    resumed.prompt("Which day?", Vec::new()).await.unwrap();
    assert_eq!(provider.requests().last().unwrap().messages.len(), 7);
}

#[tokio::test]
async fn compacts_automatically_near_the_context_window() {
    let root = tempfile::tempdir().unwrap();
    let long = "x".repeat(4000);
    let provider = FauxProvider::new([
        FauxResponse::text(format!("first answer {long}")),
        FauxResponse::text(format!("second answer {long}")),
        FauxResponse::text("## Goal\nSummarized"),
        FauxResponse::text("third answer"),
    ]);
    register_api_provider("faux-compact", provider.clone());
    let mut session = CodingSession::new(options(root.path(), "faux-compact")).await.unwrap();
    session.compaction.reserve_tokens = 2500;
    session.compaction.keep_recent_tokens = 1500;
    session.prompt(&format!("first question {long}"), Vec::new()).await.unwrap();
    session.prompt(&format!("second question {long}"), Vec::new()).await.unwrap();
    session.prompt("third question", Vec::new()).await.unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].system_prompt.as_deref().unwrap().starts_with("You are a context summarization assistant"));
    let after = &requests[3].messages;
    assert!(matches!(&after[0], Message::User(user) if user.content.text().contains("Summarized")));
    assert!(after.len() < 6, "older turns were replaced by the summary");
    let text = std::fs::read_to_string(session.session_file().unwrap()).unwrap();
    assert!(text.contains("\"type\":\"compaction\""));
}

fn retry_options(root: &Path, api: &str, retry: serde_json::Value) -> SessionOptions {
    let settings = SettingsManager::in_memory(json!({ "retry": retry }));
    SessionOptions { persist: false, settings: Some(Arc::new(settings)), ..options(root, api) }
}

fn record_events(session: &CodingSession) -> Arc<Mutex<Vec<serde_json::Value>>> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    session.subscribe(move |event| {
        let value = event.to_json();
        if value["type"].as_str().is_some_and(|kind| kind.starts_with("auto_retry") || kind == "agent_end") {
            sink.lock().unwrap().push(value);
        }
    });
    events
}

#[tokio::test]
async fn retries_transient_errors_up_to_max_retries() {
    let root = tempfile::tempdir().unwrap();
    let provider = FauxProvider::new((0..3).map(|_| FauxResponse::Error("503 service unavailable".into())));
    register_api_provider("faux-retry", provider.clone());
    let retry = json!({"baseDelayMs": 1, "maxRetries": 2});
    let session = CodingSession::new(retry_options(root.path(), "faux-retry", retry)).await.unwrap();
    let events = record_events(&session);
    session.prompt("hi", Vec::new()).await.unwrap();

    assert_eq!(provider.requests().len(), 3);
    let events = events.lock().unwrap().clone();
    let kinds: Vec<&str> = events.iter().map(|event| event["type"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["agent_end", "auto_retry_start", "agent_end", "auto_retry_start", "agent_end", "auto_retry_end"]
    );
    assert_eq!((events[1]["delayMs"].as_u64(), events[3]["delayMs"].as_u64()), (Some(1), Some(2)));
    assert_eq!(events[4]["willRetry"], false);
    assert_eq!(events[5]["success"], false);
    assert_eq!(events[5]["attempt"], 2);
    assert!(events[5]["finalError"].as_str().unwrap().contains("503"));
    let messages = session.agent.state().messages;
    assert_eq!(messages.len(), 2, "failed responses before the last one leave the context");
}

#[tokio::test]
async fn abort_cancels_a_pending_retry() {
    let root = tempfile::tempdir().unwrap();
    let provider = FauxProvider::new([FauxResponse::Error("overloaded".into())]);
    register_api_provider("faux-retry-abort", provider.clone());
    let retry = json!({"baseDelayMs": 60_000});
    let session = Arc::new(CodingSession::new(retry_options(root.path(), "faux-retry-abort", retry)).await.unwrap());
    let events = record_events(&session);
    let running = tokio::spawn({
        let session = session.clone();
        async move { session.prompt("hi", Vec::new()).await }
    });
    while !session.is_retrying() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    session.abort();
    tokio::time::timeout(Duration::from_secs(5), running).await.unwrap().unwrap().unwrap();
    let last = events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        last,
        json!({"type": "auto_retry_end", "success": false, "attempt": 1, "finalError": "Retry cancelled"})
    );
    assert_eq!(provider.requests().len(), 1);
}
