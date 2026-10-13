//! Ported from upstream `test/session-manager/build-context.test.ts`, `load-entries.test.ts`,
//! and the session half of `session-context-edit.test.ts`.

mod common;

use common::*;
use pi_ai::{ContentBlock, Message, ThinkingLevel, ToolResultMessage};
use pi_coding_agent::session::{Leaf, NewSessionOptions, SessionManager, build_context_entries, build_session_context};
use serde_json::{Value, json};
use std::path::Path;

fn custom(id: &str, parent: &str, custom_type: &str) -> Value {
    json!({"type": "custom", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z", "customType": custom_type, "data": {}})
}

fn branch_summary(id: &str, parent: &str, summary: &str, from: &str) -> Value {
    json!({"type": "branch_summary", "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z", "summary": summary, "fromId": from})
}

fn change(id: &str, parent: &str, kind: &str, fields: Value) -> Value {
    let mut entry = json!({"type": kind, "id": id, "parentId": parent, "timestamp": "2025-01-01T00:00:00Z"});
    entry.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
    entry
}

fn texts(messages: &[Message]) -> Vec<String> {
    messages.iter().map(text).collect()
}

#[test]
fn trivial_contexts_track_settings() {
    let empty = with_entries(Vec::new());
    let context = build_session_context(empty.entries(), Leaf::Last);
    assert!(context.messages.is_empty() && context.model.is_none() && context.thinking_level.is_none());
    let session = with_entries(vec![
        message_entry("1", None, &user("hello")),
        change("2", "1", "thinking_level_change", json!({"thinkingLevel": "high"})),
        change("3", "2", "model_change", json!({"provider": "openai", "modelId": "gpt-4"})),
        message_entry("4", Some("3"), &assistant("hi")),
    ]);
    let context = session.build_session_context();
    assert_eq!(roles(&context.messages), ["user", "assistant"]);
    assert_eq!(context.thinking_level, Some(ThinkingLevel::High));
    // The assistant message names the model last, so it wins over the model change.
    assert_eq!(context.model, Some(("anthropic".into(), "test".into())));
}

#[test]
fn compaction_replaces_older_entries_with_its_summary() {
    let session = with_entries(vec![
        message_entry("1", None, &user("first")),
        message_entry("2", Some("1"), &assistant("response1")),
        message_entry("3", Some("2"), &user("second")),
        message_entry("4", Some("3"), &assistant("response2")),
        compaction_entry("5", Some("4"), "Summary of first two turns", "3"),
        message_entry("6", Some("5"), &user("third")),
        message_entry("7", Some("6"), &assistant("response3")),
    ]);
    let context = session.build_session_context();
    assert_eq!(texts(&context.messages), ["Summary of first two turns", "second", "response2", "third", "response3"]);
    assert_eq!(context.messages[0].role(), "compactionSummary");

    let multiple = with_entries(vec![
        message_entry("1", None, &user("a")),
        message_entry("2", Some("1"), &assistant("b")),
        compaction_entry("3", Some("2"), "First summary", "1"),
        message_entry("4", Some("3"), &user("c")),
        message_entry("5", Some("4"), &assistant("d")),
        compaction_entry("6", Some("5"), "Second summary", "4"),
        message_entry("7", Some("6"), &user("e")),
    ]);
    assert_eq!(texts(&multiple.build_session_context().messages), ["Second summary", "c", "d", "e"]);
}

#[test]
fn context_entries_keep_custom_entries_in_the_kept_range() {
    let session = with_entries(vec![
        message_entry("1", None, &user("first")),
        custom("2", "1", "old-state"),
        message_entry("3", Some("2"), &assistant("response1")),
        custom("4", "3", "kept-card"),
        message_entry("5", Some("4"), &user("second")),
        compaction_entry("6", Some("5"), "Summary", "4"),
        custom("7", "6", "after-card"),
        message_entry("8", Some("7"), &assistant("response2")),
    ]);
    assert_eq!(ids(&build_context_entries(session.entries(), Leaf::Last)), ["6", "4", "5", "7", "8"]);
    assert_eq!(roles(&session.build_session_context().messages), ["compactionSummary", "user", "assistant"]);
}

#[test]
fn branches_and_branch_summaries_follow_the_leaf() {
    let session = with_entries(vec![
        message_entry("1", None, &user("start")),
        message_entry("2", Some("1"), &assistant("r1")),
        message_entry("3", Some("2"), &user("q2")),
        message_entry("4", Some("3"), &assistant("r2")),
        compaction_entry("5", Some("4"), "Compacted history", "3"),
        message_entry("6", Some("5"), &user("q3")),
        message_entry("7", Some("6"), &assistant("r3")),
        message_entry("8", Some("3"), &user("wrong path")),
        message_entry("9", Some("8"), &assistant("wrong response")),
        branch_summary("10", "3", "Tried wrong approach", "9"),
        message_entry("11", Some("10"), &user("better approach")),
    ]);
    let main = build_session_context(session.entries(), Leaf::Entry("7"));
    assert_eq!(texts(&main.messages), ["Compacted history", "q2", "r2", "q3", "r3"]);
    let branch = build_session_context(session.entries(), Leaf::Entry("11"));
    assert_eq!(texts(&branch.messages), ["start", "r1", "q2", "Tried wrong approach", "better approach"]);
    assert_eq!(branch.messages[3].role(), "branchSummary");
}

#[test]
fn unknown_leaves_and_orphans_degrade_gracefully() {
    let session =
        with_entries(vec![message_entry("1", None, &user("hello")), message_entry("2", Some("1"), &assistant("hi"))]);
    assert_eq!(build_session_context(session.entries(), Leaf::Entry("nonexistent")).messages.len(), 2);
    let orphan = with_entries(vec![
        message_entry("1", None, &user("hello")),
        message_entry("2", Some("missing"), &assistant("orphan")),
    ]);
    assert_eq!(build_session_context(orphan.entries(), Leaf::Entry("2")).messages.len(), 1);
}

#[test]
fn preloaded_entries_are_adopted_verbatim() {
    let mut source = memory();
    let first = source.append_message(&user("hello")).unwrap();
    source.append_message(&user("abandoned")).unwrap();
    source.branch(&first).unwrap();
    source.append_message(&user("kept")).unwrap();
    source.append_label_change(&first, Some("checkpoint")).unwrap();
    let raw = raw_entries(&source);
    let mut session =
        SessionManager::in_memory_with(Path::new("/project"), NewSessionOptions::default(), raw.clone()).unwrap();
    assert_eq!(raw_entries(&session), raw);
    assert_eq!(session.get_tree().node(session.get_tree().roots[0]).children.len(), 2);
    assert_eq!(session.get_label(&first), Some("checkpoint"));
    let last = raw.last().unwrap()["id"].as_str().unwrap().to_string();
    let appended = session.append_message(&user("continued")).unwrap();
    assert_eq!(session.get_entry(&appended).unwrap().parent_id(), Some(last.as_str()));
    assert!(!raw.iter().any(|entry| entry["id"] == appended.as_str()));
    assert!(session.session_file().is_none() && !session.is_persisted());
}

#[test]
fn preloaded_entries_take_identity_from_options_or_header() {
    let entries = raw_entries(&{
        let mut source = memory();
        source.append_message(&user("hello")).unwrap();
        source
    });
    let options = NewSessionOptions { id: Some("restored-session".into()), parent_session: None };
    let restored = SessionManager::in_memory_with(Path::new("/project"), options, entries.clone()).unwrap();
    let project = std::path::absolute("/project").unwrap().display().to_string();
    assert_eq!((restored.session_id(), restored.header().cwd.as_str()), ("restored-session", project.as_str()));
    let generated =
        SessionManager::in_memory_with(Path::new("/project"), NewSessionOptions::default(), entries.clone()).unwrap();
    assert_eq!(&generated.session_id()[14..15], "7");
    let empty = SessionManager::in_memory_with(
        Path::new("/project"),
        NewSessionOptions { id: Some("empty-session".into()), parent_session: None },
        Vec::new(),
    )
    .unwrap();
    assert_eq!((empty.session_id(), empty.entries().len(), empty.leaf_id()), ("empty-session", 0, None));

    let header = json!({"type": "session", "version": 3, "id": "stored-session", "timestamp": "2026-01-01T00:00:00Z", "cwd": "/stored"});
    let mut with_header = vec![header.as_object().unwrap().clone()];
    with_header.extend(entries);
    let ignored = NewSessionOptions { id: Some("ignored".into()), parent_session: None };
    let stored = SessionManager::in_memory_with(Path::new("/project"), ignored, with_header).unwrap();
    assert_eq!((stored.session_id(), stored.header().cwd.as_str()), ("stored-session", "/stored"));
}

#[test]
fn preloaded_entries_migrate_only_with_an_old_header() {
    let hook = json!({"type": "message", "id": "abc12345", "parentId": null, "timestamp": "2026-01-01T00:00:01Z",
        "message": {"role": "hookMessage", "content": "from a hook", "timestamp": 1}});
    let header = json!({"type": "session", "version": 2, "id": "v2-session", "timestamp": "2026-01-01T00:00:00Z", "cwd": "/project"});
    let entries = vec![header.as_object().unwrap().clone(), hook.as_object().unwrap().clone()];
    let migrated =
        SessionManager::in_memory_with(Path::new("/project"), NewSessionOptions::default(), entries).unwrap();
    assert_eq!(migrated.header().version, Some(3));
    assert_eq!((migrated.entries()[0].id(), migrated.entries()[0].message().unwrap().role()), ("abc12345", "custom"));
    let headerless = SessionManager::in_memory_with(
        Path::new("/project"),
        NewSessionOptions::default(),
        vec![hook.as_object().unwrap().clone()],
    )
    .unwrap();
    assert_eq!(headerless.entries()[0].raw()["message"]["role"], "hookMessage");
}

fn tool_result(text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "read".into(),
        content: vec![ContentBlock::text(text)],
        is_error: true,
        ..Default::default()
    })
}

#[test]
fn context_edits_omit_and_replace_only_in_the_projection() {
    let mut session = memory();
    session.append_message(&user("request")).unwrap();
    let assistant_id = session.append_message(&assistant("partial")).unwrap();
    let result_id = session.append_message(&tool_result("raw output")).unwrap();
    session.append_context_edit(&assistant_id, None).unwrap();
    session.append_context_edit(&result_id, None).unwrap();
    assert_eq!(session.get_branch(None).iter().filter(|entry| entry.kind() == "message").count(), 3);
    assert_eq!(roles(&session.build_session_projection().messages), ["user"]);
    assert_eq!(text(session.get_entry(&result_id).unwrap().message().unwrap()), "raw output");

    let mut session = memory();
    let target = session.append_message(&assistant("original")).unwrap();
    session.append_context_edit(&target, Some(json!([{"type": "text", "text": "first"}]))).unwrap();
    session.append_context_edit(&target, None).unwrap();
    session.append_context_edit(&target, Some(json!([{"type": "text", "text": "restored"}]))).unwrap();
    let projected = &session.build_session_projection().messages[0];
    assert_eq!(text(projected), "restored");
    assert!(matches!(projected, Message::Assistant(message) if message.usage.total_tokens == 2));
    assert_eq!(text(session.get_entry(&target).unwrap().message().unwrap()), "original");
}

#[test]
fn context_edits_normalize_strings_and_stay_branch_relative() {
    let mut session = memory();
    let assistant_id = session.append_message(&assistant("original")).unwrap();
    let result_id = session.append_message(&tool_result("original result")).unwrap();
    let edit = session.append_context_edit(&assistant_id, Some(json!("assistant replacement"))).unwrap();
    session.append_context_edit(&result_id, Some(json!("result replacement"))).unwrap();
    let stored = &session.get_entry(&edit).unwrap().raw()["replacement"];
    assert_eq!(stored, &json!({"content": [{"type": "text", "text": "assistant replacement"}]}));
    assert_eq!(texts(&session.build_session_projection().messages), ["assistant replacement", "result replacement"]);
    // A string replacement imported from elsewhere is normalized when projecting.
    let mut raw = raw_entries(&session);
    raw.push(
        json!({"type": "context_edit", "id": "imported", "parentId": raw.last().unwrap()["id"], "timestamp": "t",
        "targetId": assistant_id, "replacement": {"content": "imported replacement"}})
        .as_object()
        .unwrap()
        .clone(),
    );
    let imported = SessionManager::in_memory_with(Path::new("/p"), NewSessionOptions::default(), raw).unwrap();
    assert_eq!(text(&imported.build_session_projection().messages[0]), "imported replacement");

    let mut session = memory();
    let target = session.append_message(&user("original")).unwrap();
    session.append_context_edit(&target, Some(json!("edited"))).unwrap();
    assert_eq!(text(&session.build_session_projection().messages[0]), "edited");
    session.branch(&target).unwrap();
    assert_eq!(text(&session.build_session_projection().messages[0]), "original");
    let error = session.append_context_edit(&target, Some(json!(5))).unwrap_err().to_string();
    assert_eq!(error, "Context edit replacement must be null or contain string/array content");
    let model = session.append_model_change("a", "b").unwrap();
    let error = session.append_context_edit(&model, None).unwrap_err().to_string();
    assert_eq!(error, format!("Entry {model} does not contribute editable model content"));
}

#[test]
fn retain_none_and_repeated_compactions_project_one_summary() {
    let mut session = memory();
    session.append_message(&user("discarded")).unwrap();
    let compaction = session.append_compaction_with(Default::default()).unwrap();
    session.append_message(&user("after")).unwrap();
    assert_eq!(session.get_entry(&compaction).unwrap().compaction().unwrap().first_kept_entry_id, compaction);
    assert_eq!(roles(&session.build_session_projection().messages), ["compactionSummary", "user"]);

    let mut session = memory();
    session.append_message(&user("summarized")).unwrap();
    let retained = session.append_message(&user("original retained")).unwrap();
    session.append_compaction("first summary", &retained, 100).unwrap();
    session.append_context_edit(&retained, Some(json!("edited retained"))).unwrap();
    session.append_message(&assistant("after first compaction")).unwrap();
    session.append_compaction("second summary", &retained, 80).unwrap();
    let projected = texts(&session.build_session_projection().messages);
    assert_eq!(projected, ["second summary", "edited retained", "after first compaction"]);
}
