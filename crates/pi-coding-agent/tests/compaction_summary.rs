//! Summary generation with the faux provider: compaction prompts, split turns, file lists,
//! stored usage, failures, and the summarization retry.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::retry::{RetryEvent, RetryPolicy};
use pi_ai::{ContentBlock, Message, Model, StopReason, ToolCall};
use pi_coding_agent::compaction::{
    CompactionSettings, SUMMARIZATION_SYSTEM_PROMPT, SummarizationOptions, compact, compact_session, prepare_compaction,
};
use pi_coding_agent::session::{EntryData, SessionManager};
use serde_json::json;
use tokio_util::sync::CancellationToken;

pub fn faux(api: &str, responses: Vec<FauxResponse>) -> (Arc<FauxProvider>, Model) {
    let provider = FauxProvider::new(responses);
    pi_ai::register_api_provider(api, provider.clone());
    let mut model = FauxProvider::model();
    model.api = api.into();
    (provider, model)
}

fn prompt_of(provider: &FauxProvider, index: usize) -> String {
    text(&provider.requests()[index].messages[0])
}

fn tool_call(name: &str, path: &str) -> Message {
    let mut message = assistant("");
    if let Message::Assistant(assistant) = &mut message {
        assistant.content = vec![ContentBlock::ToolCall(ToolCall {
            id: format!("{name}-{path}"),
            name: name.into(),
            arguments: json!({"path": path}).as_object().unwrap().clone(),
            ..Default::default()
        })];
        assistant.stop_reason = StopReason::ToolUse;
    }
    message
}

fn long_session(turns: usize) -> SessionManager {
    let mut session = memory();
    for turn in 0..turns {
        session.append_message(&user(&format!("question {turn} {}", "x".repeat(400)))).unwrap();
        session.append_message(&assistant(&format!("answer {turn} {}", "y".repeat(400)))).unwrap();
    }
    session
}

#[tokio::test]
async fn compacts_with_upstream_prompts_and_records_the_entry() {
    let (provider, model) =
        faux("faux-compact-basic", vec![FauxResponse::text("## Goal\nsummary"), FauxResponse::text("## Goal\nsecond")]);
    let mut session = long_session(6);
    let settings = CompactionSettings { keep_recent_tokens: 200, ..CompactionSettings::default() };
    let plan = prepare_compaction(&session, &settings).unwrap();
    assert!(!plan.is_split_turn && plan.previous_summary.is_none());
    let result = compact(&mut session, plan.clone(), &model, None, Some("focus"), &settings, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.summary, "## Goal\nsummary");
    assert!(result.estimated_tokens_after.is_some_and(|after| after > 0));
    let request = &provider.requests()[0];
    assert_eq!(request.system_prompt.as_deref(), Some(SUMMARIZATION_SYSTEM_PROMPT));
    let prompt = prompt_of(&provider, 0);
    assert!(prompt.starts_with("<conversation>\n[User]: question 0"));
    assert!(prompt.contains("The messages above are a conversation to summarize."));
    assert!(prompt.ends_with("error messages.\n\nAdditional focus: focus"));

    let entry = session.leaf_entry().unwrap();
    let compaction = entry.compaction().unwrap();
    assert_eq!((compaction.from_hook, compaction.usage), (Some(false), Some(result.usage)));
    assert_eq!(compaction.details, Some(json!({"readFiles": [], "modifiedFiles": []})));
    assert_eq!(compaction.first_kept_entry_id, plan.first_kept_entry_id);
    let context = session.build_session_context();
    assert_eq!(context.messages[0].role(), "compactionSummary");
    assert_eq!(context.messages.len(), 12 - plan.messages_to_summarize.len() + 1);

    for turn in 6..9 {
        session.append_message(&user(&format!("question {turn} {}", "x".repeat(400)))).unwrap();
        session.append_message(&assistant(&format!("answer {turn}"))).unwrap();
    }
    let plan = prepare_compaction(&session, &settings).unwrap();
    assert_eq!(plan.previous_summary.as_deref(), Some("## Goal\nsummary"));
    compact(&mut session, plan, &model, None, None, &settings, CancellationToken::new()).await.unwrap();
    let prompt = prompt_of(&provider, 1);
    assert!(prompt.contains("<previous-summary>\n## Goal\nsummary\n</previous-summary>\n\nThe messages above are NEW"));
    assert!(!prompt.contains("Additional focus"));
}

#[tokio::test]
async fn split_turns_merge_a_turn_prefix_summary_and_file_lists() {
    let (provider, model) =
        faux("faux-compact-split", vec![FauxResponse::text("history summary"), FauxResponse::text("prefix summary")]);
    let mut session = memory();
    session.append_message(&user("earlier request")).unwrap();
    session.append_message(&tool_call("edit", "src/old.rs")).unwrap();
    session.append_message(&user("one long turn")).unwrap();
    session.append_message(&tool_call("read", "src/a.rs")).unwrap();
    session.append_message(&tool_call("write", "src/b.rs")).unwrap();
    session.append_message(&assistant(&"final answer ".repeat(200))).unwrap();
    let settings = CompactionSettings { keep_recent_tokens: 100, ..CompactionSettings::default() };
    let plan = prepare_compaction(&session, &settings).unwrap();
    assert!(plan.is_split_turn);
    assert_eq!(plan.turn_prefix_messages.len(), 3);
    let options = SummarizationOptions::default();
    let result = compact_session(&mut session, &plan, &model, None, &options).await.unwrap();
    assert_eq!(provider.requests().len(), 2);
    assert!(prompt_of(&provider, 1).starts_with("# Conversation\n[User]: one long turn"));
    assert!(prompt_of(&provider, 1).contains("# Instructions\nThe messages above are earlier context"));
    assert!(result.summary.starts_with("history summary\n\n---\n\n**Turn Context (split turn):**\n\nprefix summary"));
    assert!(result.summary.ends_with(
        "<read-files>\nsrc/a.rs\n</read-files>\n\n<modified-files>\nsrc/b.rs\nsrc/old.rs\n</modified-files>"
    ));
    assert_eq!(result.details, Some(json!({"readFiles": ["src/a.rs"], "modifiedFiles": ["src/b.rs", "src/old.rs"]})));
    assert!(result.usage.output >= 2, "usage of both calls is combined");
    let stored = session.leaf_entry().unwrap().compaction().unwrap().clone();
    assert_eq!(stored.usage, Some(result.usage));
    let stats = session.session_stats();
    assert_eq!(stats.tokens.output, result.usage.output + 4);
}

#[tokio::test]
async fn rejects_truncated_or_tool_calling_summaries() {
    let length = FauxResponse::Message { content: vec![ContentBlock::text("partial")], stop: StopReason::Length };
    let (_, model) =
        faux("faux-compact-fail", vec![length, FauxResponse::tool_call("t", "read", json!({"path": "x"}))]);
    let mut session = long_session(6);
    let settings = CompactionSettings { keep_recent_tokens: 200, ..CompactionSettings::default() };
    let plan = prepare_compaction(&session, &settings).unwrap();
    let error = compact(&mut session, plan.clone(), &model, None, None, &settings, CancellationToken::new()).await;
    assert_eq!(
        error.unwrap_err().to_string(),
        "Summarization failed: generation hit the token cap and the summary is incomplete"
    );
    let error = compact(&mut session, plan, &model, None, None, &settings, CancellationToken::new()).await;
    assert_eq!(error.unwrap_err().to_string(), "Summarization attempted to call a tool");
    assert!(!session.entries().iter().any(|entry| matches!(entry.data(), EntryData::Compaction(_))));
}

#[tokio::test]
async fn retries_transient_summarization_errors() {
    let (provider, model) = faux(
        "faux-compact-retry",
        vec![FauxResponse::Error("socket hang up".into()), FauxResponse::text("recovered summary")],
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let options = SummarizationOptions {
        retry: Some(RetryPolicy { enabled: true, max_retries: 3, base_delay_ms: 1, max_agent_delay_ms: None }),
        on_retry: Some(Arc::new(move |event| {
            sink.lock().unwrap().push(match event {
                RetryEvent::Scheduled { retry, max_retries, error_message, .. } => {
                    format!("scheduled {retry}/{max_retries}: {error_message}")
                }
                RetryEvent::RetryStart => "start".into(),
                RetryEvent::Finished { success, retry, .. } => format!("finished {success} {retry}"),
            })
        })),
        ..SummarizationOptions::default()
    };
    let mut session = long_session(6);
    let settings = CompactionSettings { keep_recent_tokens: 200, ..CompactionSettings::default() };
    let plan = prepare_compaction(&session, &settings).unwrap();
    let result = compact_session(&mut session, &plan, &model, None, &options).await.unwrap();
    assert_eq!(result.summary, "recovered summary");
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(*events.lock().unwrap(), ["scheduled 1/3: socket hang up", "start", "finished true 1"]);

    let (provider, model) = faux("faux-compact-no-retry", vec![FauxResponse::Error("socket hang up".into())]);
    let error = compact_session(&mut session, &plan, &model, None, &SummarizationOptions::default()).await;
    assert_eq!(error.unwrap_err().to_string(), "Summarization failed: socket hang up");
    assert_eq!(provider.requests().len(), 1);
}
