//! Cut points, split turns, and preparation, ported from upstream `test/compaction.test.ts`
//! and the compaction half of `session-context-edit.test.ts`.

mod common;

use common::*;
use pi_ai::{ContentBlock, Message, StopReason, SystemMessage, ToolCall, ToolResultMessage, UserContent};
use pi_coding_agent::compaction::{
    CompactionSettings, calculate_context_tokens, estimate_context_tokens, estimate_projected_context_tokens,
    find_cut_point, last_assistant_usage, prepare_compaction, prepare_compaction_entries, should_compact,
};
use pi_coding_agent::session::{Entry, SessionManager};
use serde_json::json;

fn keep(tokens: u64) -> CompactionSettings {
    CompactionSettings { keep_recent_tokens: tokens, ..CompactionSettings::default() }
}

fn chain(messages: &[Message]) -> SessionManager {
    let mut session = memory();
    for message in messages {
        session.append_message(message).unwrap();
    }
    session
}

fn joined(messages: &[Message]) -> String {
    messages.iter().map(text).collect::<Vec<_>>().join("\n")
}

#[test]
fn token_math_and_thresholds() {
    assert_eq!(calculate_context_tokens(&usage(1000, 500, 200, 100)), 1800);
    assert_eq!(calculate_context_tokens(&usage(0, 0, 0, 0)), 0);
    let mut aborted = assistant_with("Aborted", usage(300, 150, 0, 0));
    if let Message::Assistant(message) = &mut aborted {
        message.stop_reason = StopReason::Aborted;
    }
    let session = chain(&[
        user("Hello"),
        assistant_with("Hi", usage(100, 50, 0, 0)),
        user("How are you?"),
        aborted,
        assistant_with("Partial", usage(0, 0, 0, 0)),
    ]);
    assert_eq!(last_assistant_usage(session.entries().iter()).map(|usage| usage.input), Some(100));
    assert_eq!(last_assistant_usage(chain(&[user("Hello")]).entries().iter()), None);
    let estimate = estimate_context_tokens(&[
        user("Hello"),
        assistant_with("Hi", usage(100, 50, 0, 0)),
        user("continue"),
        assistant_with("Partial thinking", usage(0, 0, 0, 0)),
    ]);
    assert_eq!((estimate.usage_tokens, estimate.last_usage_index), (150, Some(1)));
    assert!(estimate.trailing_tokens > 0 && estimate.tokens == 150 + estimate.trailing_tokens);
    let settings = CompactionSettings { enabled: true, reserve_tokens: 10_000, keep_recent_tokens: 20_000 };
    assert!(should_compact(95_000, 100_000, &settings) && !should_compact(89_000, 100_000, &settings));
    assert!(!should_compact(95_000, 100_000, &CompactionSettings { enabled: false, ..settings }));
    assert!(!should_compact(95_000, 0, &settings), "an unknown window does not trigger compaction");
}

#[test]
fn cut_points_land_on_user_or_assistant_messages() {
    let mut messages = Vec::new();
    for turn in 0..10 {
        messages.push(user(&format!("User {turn}")));
        messages.push(assistant_with(&format!("Assistant {turn}"), usage(0, 100, (turn + 1) * 1000, 0)));
    }
    let session = chain(&messages);
    let branch = session.get_branch(None);
    let result = find_cut_point(&branch, 0, branch.len(), 2500);
    let role = branch[result.first_kept_entry_index].message().unwrap().role();
    assert!(role == "user" || role == "assistant");
    let single = chain(&[assistant("a")]);
    assert_eq!(find_cut_point(&single.get_branch(None), 0, 1, 1000).first_kept_entry_index, 0);
    let small = chain(&[user("1"), assistant("a"), user("2"), assistant("b")]);
    assert_eq!(find_cut_point(&small.get_branch(None), 0, 4, 50_000).first_kept_entry_index, 0);
}

#[test]
fn split_turns_report_their_turn_start() {
    let session = chain(&[
        user("Turn 1"),
        assistant("A1"),
        user("Turn 2"),
        assistant(&"A2-1 ".repeat(1000)),
        assistant(&"A2-2 ".repeat(1000)),
        assistant(&"A2-3 ".repeat(1000)),
    ]);
    let branch = session.get_branch(None);
    let result = find_cut_point(&branch, 0, branch.len(), 3000);
    assert_eq!((result.first_kept_entry_index, result.turn_start_index, result.is_split_turn), (3, Some(2), true));
}

#[test]
fn custom_messages_count_toward_the_budget() {
    let mut session = chain(&[user("hi"), assistant("hello")]);
    session.append_custom_message_entry("test", UserContent::Text("x".repeat(4000)), true, None).unwrap();
    session.append_message(&assistant("ok")).unwrap();
    let branch = session.get_branch(None);
    let tiny = find_cut_point(&branch, 0, branch.len(), 1);
    assert_eq!((tiny.first_kept_entry_index, tiny.turn_start_index, tiny.is_split_turn), (3, Some(2), true));
    let fits = find_cut_point(&branch, 0, branch.len(), 2);
    assert_eq!((fits.first_kept_entry_index, fits.turn_start_index, fits.is_split_turn), (2, None, false));
}

#[test]
fn oversized_tool_results_keep_their_call() {
    let mut call = assistant("");
    if let Message::Assistant(message) = &mut call {
        message.content = vec![ContentBlock::ToolCall(ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            arguments: json!({"path": "big.txt"}).as_object().unwrap().clone(),
            ..Default::default()
        })];
        message.stop_reason = StopReason::ToolUse;
    }
    let result = Message::ToolResult(ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text("x".repeat(8000))],
        ..Default::default()
    });
    let session = chain(&[user("old history"), assistant("old answer"), user("read the large file"), call, result]);
    let branch = session.get_branch(None);
    let cut = find_cut_point(&branch, 0, branch.len(), 1000);
    assert_eq!((cut.first_kept_entry_index, cut.turn_start_index, cut.is_split_turn), (3, Some(2), true));
    let preparation = prepare_compaction(&session, &keep(1000)).unwrap();
    assert_eq!(preparation.first_kept_entry_id, branch[3].id());
    assert_eq!(joined(&preparation.messages_to_summarize), "old history\nold answer");
    assert_eq!(joined(&preparation.turn_prefix_messages), "read the large file");
    assert!(preparation.file_ops.read.is_empty(), "the kept tool call is not summarized");
}

#[test]
fn system_messages_are_not_history() {
    let system = Message::System(SystemMessage {
        content: UserContent::Text(String::new()),
        sections: Some(json!({"preamble": "current prompt"}).as_object().unwrap().clone()),
        tools_added: None,
        tools_removed: None,
        timestamp: 1,
    });
    let session = chain(&[system, user("one long turn"), assistant("assistant suffix")]);
    let preparation = prepare_compaction(&session, &keep(1)).unwrap();
    assert_eq!(preparation.first_kept_entry_id, session.entries()[2].id());
    assert!(preparation.is_split_turn && preparation.messages_to_summarize.is_empty());
    assert_eq!(joined(&preparation.turn_prefix_messages), "one long turn");
}

#[test]
fn repeated_compactions_extend_the_previous_summary() {
    let build = |scale: usize| {
        let mut session = memory();
        session.append_message(&user(&"user msg 1 (summarized by compaction1) ".repeat(scale))).unwrap();
        session.append_message(&assistant(&"assistant msg 1 ".repeat(scale))).unwrap();
        let kept = session.append_message(&user(&"user msg 2 - kept by compaction1 ".repeat(scale * 3))).unwrap();
        session.append_message(&assistant(&"assistant msg 2 ".repeat(scale * 3))).unwrap();
        session.append_message(&user(&"user msg 3 - kept by compaction1 ".repeat(scale * 3))).unwrap();
        session
            .append_message(&assistant_with(&"assistant msg 3 ".repeat(scale * 3), usage(5000, 1000, 0, 0)))
            .unwrap();
        session.append_compaction("First summary", &kept, 10_000).unwrap();
        session.append_message(&user(&"user msg 4 (new after compaction1) ".repeat(scale * 3))).unwrap();
        session
            .append_message(&assistant_with(&"assistant msg 4 ".repeat(scale * 3), usage(8000, 2000, 0, 0)))
            .unwrap();
        session
    };
    assert!(prepare_compaction(&build(1), &CompactionSettings::default()).is_none());
    let preparation = prepare_compaction(&build(4), &keep(100)).unwrap();
    let summarized = joined(&preparation.messages_to_summarize);
    assert!(summarized.contains("user msg 2 - kept by compaction1") && summarized.contains("user msg 3 - kept"));
    assert!(!summarized.contains("First summary"));
    assert_eq!(preparation.previous_summary.as_deref(), Some("First summary"));
    let mut done = build(4);
    let leaf = done.leaf_id().unwrap().to_string();
    done.append_compaction("Second", &leaf, 1).unwrap();
    assert!(prepare_compaction(&done, &keep(100)).is_none(), "a branch ending in a compaction is done");
}

#[test]
fn projected_estimates_distrust_usage_before_edits_and_compactions() {
    let mut session = memory();
    let large = session.append_message(&user(&"discarded input ".repeat(2000))).unwrap();
    let answer = session.append_message(&assistant_with("small answer", usage(10_000, 1, 0, 0))).unwrap();
    session.append_context_edit(&large, None).unwrap();
    let estimate = estimate_projected_context_tokens(&session.build_session_projection(), &session.get_branch(None));
    assert!(estimate.usage_tokens == 0 && estimate.tokens < 100);
    session.append_context_edit(&answer, None).unwrap();
    assert_eq!(
        estimate_projected_context_tokens(&session.build_session_projection(), &session.get_branch(None)).tokens,
        0
    );

    let mut session = memory();
    let input = session.append_message(&user("original")).unwrap();
    session.append_context_edit(&input, Some(json!("edited"))).unwrap();
    session.append_message(&assistant_with("answer", usage(4000, 100, 0, 0))).unwrap();
    session.append_message(&user("next")).unwrap();
    let estimate = estimate_projected_context_tokens(&session.build_session_projection(), &session.get_branch(None));
    assert_eq!((estimate.usage_tokens, estimate.trailing_tokens, estimate.tokens), (4100, 1, 4101));
    session.append_compaction("small summary", &input, 4100).unwrap();
    let estimate = estimate_projected_context_tokens(&session.build_session_projection(), &session.get_branch(None));
    assert!(estimate.usage_tokens == 0 && estimate.tokens < 100);
}

fn prepared(session: &SessionManager) -> Option<pi_coding_agent::compaction::CompactionPreparation> {
    let branch: Vec<&Entry> = session.get_branch(None);
    prepare_compaction_entries(&branch, &keep(1))
}

#[test]
fn boundary_input_stays_verbatim_unless_a_recovery_response_follows() {
    let mut session = chain(&[user("old request"), assistant("old answer")]);
    let replaced = session.append_message(&user("original input")).unwrap();
    let answered = session.append_message(&assistant("answered original input")).unwrap();
    session.append_context_edit(&replaced, Some(json!("NEW-INSTRUCTION ".repeat(100)))).unwrap();
    session.append_context_edit(&answered, None).unwrap();
    session.append_custom_entry("bookkeeping", Some(json!({"source": "test"}))).unwrap();
    let preparation = prepared(&session).unwrap();
    assert_eq!(preparation.first_kept_entry_id, replaced);
    assert!(!joined(&preparation.messages_to_summarize).contains("NEW-INSTRUCTION"));

    let mut session = chain(&[user("old request"), assistant("old answer")]);
    let unsent = session
        .append_custom_message_entry("next-work", UserContent::Text("UNSENT ".repeat(100)), false, None)
        .unwrap();
    session.append_custom_entry("bookkeeping", None).unwrap();
    assert_eq!(prepared(&session).unwrap().first_kept_entry_id, unsent);

    let mut session = chain(&[user(&"unanswered input ".repeat(100))]);
    let temporary =
        session.append_custom_message_entry("temporary", UserContent::Text("temp".into()), false, None).unwrap();
    session.append_context_edit(&temporary, None).unwrap();
    assert!(prepared(&session).is_none());

    let mut session = chain(&[user(&"recovery input ".repeat(100))]);
    let attempt = session.append_message(&assistant("failed attempt")).unwrap();
    session.append_context_edit(&attempt, None).unwrap();
    session.append_custom_entry("bookkeeping", None).unwrap();
    let preparation = prepared(&session).unwrap();
    assert_eq!(preparation.first_kept_entry_id, attempt);
    assert!(joined(&preparation.turn_prefix_messages).contains("recovery input"));
    assert!(preparation.messages_to_summarize.is_empty());

    let mut session = memory();
    let omitted = session.append_message(&user(&"OMIT-ME ".repeat(100))).unwrap();
    session.append_message(&assistant(&"old answer ".repeat(100))).unwrap();
    session.append_context_edit(&omitted, None).unwrap();
    session.append_message(&user("keep")).unwrap();
    session.append_message(&assistant("suffix")).unwrap();
    let preparation = prepared(&session).unwrap();
    let all = joined(&preparation.messages_to_summarize) + &joined(&preparation.turn_prefix_messages);
    assert!(!all.contains("OMIT-ME"));
}
