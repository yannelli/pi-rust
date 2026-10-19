//! Branch summaries and tree navigation, ported from upstream `branch-summarization.test.ts`
//! and `agent-session-tree-navigation.test.ts`, using the faux provider.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use pi_agent::StreamFn;
use pi_ai::providers::faux::{FauxProvider, FauxResponse};
use pi_ai::{ApiProvider, ContentBlock, StopReason, StreamOptions};
use pi_coding_agent::compaction::branch::ProvidedSummary;
use pi_coding_agent::compaction::{
    BranchSummaryOptions, SummarizationOptions, TreeNavigationRequest, collect_entries_for_branch_summary,
    generate_branch_summary, navigate_tree, prepare_branch_entries,
};
use pi_coding_agent::session::{Entry, SessionManager};
use serde_json::json;

/// A stream function over a faux provider that records each request's options.
fn recording(responses: Vec<FauxResponse>) -> (StreamFn, Arc<Mutex<Vec<StreamOptions>>>) {
    let provider = FauxProvider::new(responses);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let stream_fn: StreamFn = Arc::new(move |model, context, options| {
        log.lock().unwrap().push(options.clone());
        provider.stream(model, context, options)
    });
    (stream_fn, seen)
}

fn options(stream_fn: StreamFn) -> BranchSummaryOptions {
    BranchSummaryOptions {
        summarization: SummarizationOptions { stream_fn: Some(stream_fn), ..Default::default() },
        ..Default::default()
    }
}

fn abandoned() -> SessionManager {
    let mut session = memory();
    session.append_message(&user("Abandoned request")).unwrap();
    session
}

#[tokio::test]
async fn leaves_output_to_the_provider_and_rejects_bad_summaries() {
    let session = abandoned();
    let entries: Vec<&Entry> = session.entries().iter().collect();
    let model = FauxProvider::model();
    let (stream_fn, seen) = recording(vec![FauxResponse::text("summary")]);
    let result = generate_branch_summary(&entries, &model, &options(stream_fn.clone())).await;
    assert!(result.summary.unwrap().starts_with("The user explored a different conversation branch"));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[0].max_tokens, None, "the provider sizes summary output from the request");
    assert!(seen[0].tool_choice.is_none() && seen[0].reasoning.is_none());
    assert_eq!(seen[0].cache_retention, Some(pi_ai::CacheRetention::None));
    assert!(seen[0].session_id.is_some(), "branch summaries get a fresh routing id");

    let (stream_fn, _) = recording(vec![FauxResponse::tool_call("tool-call-1", "read", json!({"path": "README.md"}))]);
    let result = generate_branch_summary(&entries, &model, &options(stream_fn)).await;
    assert_eq!(result.error.as_deref(), Some("Branch summarization attempted to call a tool"));
    let length = FauxResponse::Message { content: vec![ContentBlock::text("partial")], stop: StopReason::Length };
    let (stream_fn, _) = recording(vec![length]);
    let result = generate_branch_summary(&entries, &model, &options(stream_fn)).await;
    assert_eq!(
        result.error.as_deref(),
        Some("Branch summarization failed: generation hit the token cap and the summary is incomplete")
    );
}

#[test]
fn collects_the_abandoned_branch_and_budgets_newest_first() {
    let mut session = memory();
    let root = session.append_message(&user("root")).unwrap();
    let left = session.append_message(&assistant("left")).unwrap();
    let left_tail = session.append_message(&user(&"left tail ".repeat(400))).unwrap();
    session.branch(&root).unwrap();
    let right = session.append_message(&assistant("right")).unwrap();
    let (entries, ancestor) = collect_entries_for_branch_summary(&session, Some(&left_tail), &right);
    assert_eq!(ancestor.as_deref(), Some(root.as_str()));
    assert_eq!(ids(&entries), [left.clone(), left_tail]);
    assert!(collect_entries_for_branch_summary(&session, None, &right).0.is_empty());

    let budgeted = prepare_branch_entries(&entries, 500);
    assert_eq!(budgeted.messages.len(), 0, "the newest message alone exceeds the budget");
    let all = prepare_branch_entries(&entries, 0);
    assert_eq!(all.messages.len(), 2);

    let mut nested = memory();
    let start = nested.append_message(&user("start")).unwrap();
    let details = json!({"readFiles": ["a.rs"], "modifiedFiles": ["b.rs"]});
    nested.branch_with_summary(Some(&start), "older summary", Some(details), Some(false), None).unwrap();
    nested.append_message(&tool_call_message("read", "c.rs")).unwrap();
    let entries: Vec<&Entry> = nested.get_branch(None).into_iter().skip(1).collect();
    let preparation = prepare_branch_entries(&entries, 0);
    assert_eq!(roles(&preparation.messages), ["branchSummary", "assistant"]);
    let reads: Vec<&String> = preparation.file_ops.read.iter().collect();
    assert_eq!(reads, ["a.rs", "c.rs"]);
    assert!(preparation.file_ops.edited.contains("b.rs"));
}

fn tool_call_message(name: &str, path: &str) -> pi_ai::Message {
    let mut message = assistant("");
    if let pi_ai::Message::Assistant(assistant) = &mut message {
        assistant.content = vec![ContentBlock::ToolCall(pi_ai::ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments: json!({"path": path}).as_object().unwrap().clone(),
            ..Default::default()
        })];
    }
    message
}

fn tree() -> (SessionManager, [String; 5]) {
    let mut session = memory();
    let first = session.append_message(&user("first question")).unwrap();
    let answer = session.append_message(&assistant("first answer")).unwrap();
    let second = session.append_message(&user("second question")).unwrap();
    session.append_message(&tool_call_message("edit", "src/lib.rs")).unwrap();
    let leaf = session.append_message(&assistant("second answer")).unwrap();
    (session, [first, answer, second, String::new(), leaf])
}

#[tokio::test]
async fn navigating_to_a_user_message_summarizes_and_returns_its_text() {
    let (mut session, [_, answer, second, _, leaf]) = tree();
    let (stream_fn, _) = recording(vec![FauxResponse::text("## Goal\nexplored")]);
    let request = TreeNavigationRequest {
        target_id: second.clone(),
        summarize: true,
        label: Some("retry".into()),
        provided_summary: None,
    };
    let model = FauxProvider::model();
    let result = navigate_tree(&mut session, request, Some(&model), &options(stream_fn)).await.unwrap();
    assert_eq!(result.editor_text.as_deref(), Some("second question"));
    let summary_id = result.summary_entry_id.unwrap();
    let entry = session.get_entry(&summary_id).unwrap();
    assert_eq!(entry.parent_id(), Some(answer.as_str()));
    let summary = entry.branch_summary().unwrap();
    assert_eq!(summary.from_id, leaf);
    assert!(summary.summary.starts_with("The user explored a different conversation branch before returning here."));
    assert!(summary.summary.ends_with("<modified-files>\nsrc/lib.rs\n</modified-files>"));
    assert_eq!(summary.details, Some(json!({"readFiles": [], "modifiedFiles": ["src/lib.rs"]})));
    assert_eq!(summary.from_hook, Some(false));
    assert_eq!(session.get_label(&summary_id), Some("retry"));
    let context = session.build_session_context();
    assert_eq!(roles(&context.messages), ["user", "assistant", "branchSummary"]);
}

#[tokio::test]
async fn navigation_without_summary_moves_the_leaf_or_resets_it() {
    let (mut session, [first, answer, _, _, leaf]) = tree();
    let model = FauxProvider::model();
    let options = BranchSummaryOptions::default();
    let unchanged = navigate_tree(
        &mut session,
        TreeNavigationRequest { target_id: leaf.clone(), ..Default::default() },
        Some(&model),
        &options,
    )
    .await
    .unwrap();
    assert_eq!(unchanged, Default::default());
    let request = TreeNavigationRequest { target_id: answer.clone(), label: Some("here".into()), ..Default::default() };
    let result = navigate_tree(&mut session, request, None, &options).await.unwrap();
    assert_eq!((result.editor_text, result.summary_entry_id), (None, None));
    assert_eq!(session.get_label(&answer), Some("here"));
    assert_eq!(session.get_branch(None).iter().filter(|entry| entry.kind() == "message").count(), 2);
    let request = TreeNavigationRequest { target_id: first.clone(), ..Default::default() };
    let result = navigate_tree(&mut session, request, None, &options).await.unwrap();
    assert_eq!(result.editor_text.as_deref(), Some("first question"));
    assert_eq!(session.leaf_id(), None);
    let missing = TreeNavigationRequest { target_id: "nope".into(), ..Default::default() };
    assert_eq!(
        navigate_tree(&mut session, missing, None, &options).await.unwrap_err().to_string(),
        "Entry nope not found"
    );
}

#[tokio::test]
async fn extension_summaries_and_missing_models() {
    let (mut session, [_, answer, ..]) = tree();
    let provided =
        ProvidedSummary { summary: "from extension".into(), details: Some(json!({"custom": 1})), usage: None };
    let request = TreeNavigationRequest {
        target_id: answer.clone(),
        summarize: true,
        label: None,
        provided_summary: Some(provided),
    };
    let result = navigate_tree(&mut session, request, None, &BranchSummaryOptions::default()).await.unwrap();
    let summary = session.get_entry(&result.summary_entry_id.unwrap()).unwrap().branch_summary().unwrap().clone();
    assert_eq!((summary.summary.as_str(), summary.from_hook), ("from extension", Some(true)));

    let (mut session, [_, answer, ..]) = tree();
    let request = TreeNavigationRequest { target_id: answer, summarize: true, ..Default::default() };
    let error = navigate_tree(&mut session, request, None, &BranchSummaryOptions::default()).await.unwrap_err();
    assert_eq!(error.to_string(), "No model available for summarization");
}

#[tokio::test]
async fn aborted_summaries_cancel_navigation() {
    let (mut session, [_, answer, ..]) = tree();
    let leaf = session.leaf_id().unwrap().to_string();
    let (stream_fn, _) = recording(vec![FauxResponse::Hang]);
    let options = options(stream_fn);
    options.summarization.cancel.cancel();
    let request = TreeNavigationRequest { target_id: answer, summarize: true, ..Default::default() };
    let result = navigate_tree(&mut session, request, Some(&FauxProvider::model()), &options).await.unwrap();
    assert!(result.cancelled && result.aborted);
    assert_eq!(session.leaf_id(), Some(leaf.as_str()));
}
