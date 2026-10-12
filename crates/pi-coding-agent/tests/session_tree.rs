//! Ported from upstream `test/session-manager/tree-traversal.test.ts`, `labels.test.ts`,
//! and `save-entry.test.ts`.

mod common;

use common::*;
use pi_ai::ThinkingLevel;
use pi_coding_agent::session::EntryData;
use serde_json::json;

#[test]
fn appends_chain_parent_ids_and_advance_the_leaf() {
    let mut session = memory();
    assert_eq!(session.leaf_id(), None);
    let id1 = session.append_message(&user("first")).unwrap();
    assert_eq!(session.leaf_id(), Some(id1.as_str()));
    let id2 = session.append_message(&assistant("second")).unwrap();
    let thinking = session.append_thinking_level_change(ThinkingLevel::High).unwrap();
    assert_eq!(session.leaf_id(), Some(thinking.as_str()));
    let model = session.append_model_change("openai", "gpt-4").unwrap();
    let id3 = session.append_message(&user("third")).unwrap();
    let entries = session.entries();
    assert_eq!(entries.len(), 5);
    assert_eq!((entries[0].id(), entries[0].parent_id(), entries[0].kind()), (id1.as_str(), None, "message"));
    assert_eq!(entries[1].parent_id(), Some(id1.as_str()));
    assert_eq!(entries[2].parent_id(), Some(id2.as_str()));
    assert!(
        matches!(entries[3].data(), EntryData::ModelChange { provider, model_id } if provider == "openai" && model_id == "gpt-4")
    );
    assert_eq!(entries[4].parent_id(), Some(model.as_str()));
    assert_eq!(entries[4].id(), id3);
    assert_eq!(entries[0].id().len(), 8);
}

#[test]
fn compaction_and_custom_entries_join_the_tree() {
    let mut session = memory();
    let id1 = session.append_message(&user("1")).unwrap();
    let id2 = session.append_message(&assistant("2")).unwrap();
    let summary_usage = usage(10, 20, 30, 40);
    let compaction = session
        .append_compaction_with(pi_coding_agent::session::NewCompaction {
            summary: "summary".into(),
            first_kept_entry_id: Some(id1.clone()),
            tokens_before: 1000,
            from_hook: Some(false),
            usage: Some(summary_usage),
            ..Default::default()
        })
        .unwrap();
    let custom = session.append_custom_entry("my_data", Some(json!({"key": "value"}))).unwrap();
    session.append_message(&user("3")).unwrap();
    let entry = session.get_entry(&compaction).unwrap();
    assert_eq!(entry.parent_id(), Some(id2.as_str()));
    let data = entry.compaction().unwrap();
    assert_eq!(
        (data.summary.as_str(), data.first_kept_entry_id.as_str(), data.tokens_before),
        ("summary", id1.as_str(), 1000)
    );
    assert_eq!(data.usage, Some(summary_usage));
    let custom_entry = session.get_entry(&custom).unwrap();
    assert!(
        matches!(custom_entry.data(), EntryData::Custom { custom_type, data } if custom_type == "my_data" && data == &Some(json!({"key": "value"})))
    );
    assert_eq!(custom_entry.parent_id(), Some(compaction.as_str()));
    assert_eq!(session.entries()[4].parent_id(), Some(custom.as_str()));
    // Custom entries stay out of the model context.
    assert_eq!(roles(&session.build_session_context().messages), ["compactionSummary", "user", "assistant", "user"]);
}

#[test]
fn get_branch_walks_from_leaf_or_given_entry() {
    let mut session = memory();
    assert!(session.get_branch(None).is_empty());
    let id1 = session.append_message(&user("1")).unwrap();
    let id2 = session.append_message(&assistant("2")).unwrap();
    let id3 = session.append_thinking_level_change(ThinkingLevel::High).unwrap();
    let id4 = session.append_message(&user("3")).unwrap();
    assert_eq!(ids(&session.get_branch(None)), [id1.clone(), id2.clone(), id3, id4]);
    assert_eq!(ids(&session.get_branch(Some(&id2))), [id1, id2]);
}

#[test]
fn get_tree_builds_branches() {
    let mut session = memory();
    assert!(session.get_tree().is_empty());
    let id1 = session.append_message(&user("1")).unwrap();
    let id2 = session.append_message(&assistant("2")).unwrap();
    let id3 = session.append_message(&user("3")).unwrap();
    session.append_message(&assistant("4")).unwrap();
    session.branch(&id2).unwrap();
    let id5 = session.append_message(&user("5")).unwrap();
    session.append_message(&assistant("6")).unwrap();
    session.branch(&id5).unwrap();
    session.append_message(&user("7")).unwrap();

    let tree = session.get_tree();
    assert_eq!(tree.roots.len(), 1);
    let root = tree.node(tree.roots[0]);
    assert_eq!(root.entry.id(), id1);
    let node2 = tree.node(root.children[0]);
    assert_eq!(node2.entry.id(), id2);
    assert_eq!(node2.children.len(), 2);
    let child_ids: Vec<&str> = node2.children.iter().map(|child| tree.node(*child).entry.id()).collect();
    assert!(child_ids.contains(&id3.as_str()) && child_ids.contains(&id5.as_str()));
    let node5 = tree.node(tree.find(&id5).unwrap());
    assert_eq!(node5.children.len(), 2);
    assert_eq!(tree.node(tree.find(&id3).unwrap()).children.len(), 1);
    assert_eq!(session.get_children(&id2).len(), 2);
}

#[test]
fn branch_moves_the_leaf_and_rejects_unknown_entries() {
    let mut session = memory();
    let id1 = session.append_message(&user("1")).unwrap();
    session.append_message(&assistant("2")).unwrap();
    session.branch(&id1).unwrap();
    assert_eq!(session.leaf_id(), Some(id1.as_str()));
    let branched = session.append_message(&user("branched")).unwrap();
    assert_eq!(session.get_entry(&branched).unwrap().parent_id(), Some(id1.as_str()));
    assert_eq!(session.branch("nonexistent").unwrap_err().to_string(), "Entry nonexistent not found");
    session.reset_leaf();
    let root = session.append_message(&user("new root")).unwrap();
    assert_eq!(session.get_entry(&root).unwrap().parent_id(), None);
    assert_eq!(session.get_tree().roots.len(), 2);
}

#[test]
fn branch_with_summary_records_source_and_usage() {
    let mut session = memory();
    let id1 = session.append_message(&user("1")).unwrap();
    session.append_message(&assistant("2")).unwrap();
    let id3 = session.append_message(&user("3")).unwrap();
    let summary_usage = usage(10, 20, 30, 40);
    let summary_id = session
        .branch_with_summary(Some(&id1), "Summary of abandoned work", None, Some(false), Some(summary_usage))
        .unwrap();
    assert_eq!(session.leaf_id(), Some(summary_id.as_str()));
    let entry = session.get_entry(&summary_id).unwrap();
    assert_eq!(entry.parent_id(), Some(id1.as_str()));
    let summary = entry.branch_summary().unwrap();
    assert_eq!((summary.from_id.as_str(), summary.summary.as_str()), (id3.as_str(), "Summary of abandoned work"));
    assert_eq!(summary.usage, Some(summary_usage));
    let error = session.branch_with_summary(Some("nonexistent"), "summary", None, None, None).unwrap_err();
    assert_eq!(error.to_string(), "Entry nonexistent not found");
}

#[test]
fn entry_lookup_and_context_follow_the_current_branch() {
    let mut session = memory();
    assert!(session.leaf_entry().is_none() && session.get_entry("nonexistent").is_none());
    session.append_message(&user("msg1")).unwrap();
    let id2 = session.append_message(&assistant("msg2")).unwrap();
    session.append_message(&user("msg3")).unwrap();
    session.branch(&id2).unwrap();
    session.append_message(&assistant("msg4-branch")).unwrap();
    let context = session.build_session_context();
    assert_eq!(context.messages.iter().map(text).collect::<Vec<_>>(), ["msg1", "msg2", "msg4-branch"]);
    assert_eq!(text(session.get_entry(&id2).unwrap().message().unwrap()), "msg2");
}

#[test]
fn create_branched_session_in_memory_keeps_only_the_path() {
    let mut session = memory();
    session.append_message(&user("hello")).unwrap();
    assert_eq!(session.create_branched_session("nonexistent").unwrap_err().to_string(), "Entry nonexistent not found");
    let mut session = memory();
    let id1 = session.append_message(&user("1")).unwrap();
    let id2 = session.append_message(&assistant("2")).unwrap();
    session.append_message(&user("3")).unwrap();
    session.branch(&id2).unwrap();
    let id4 = session.append_message(&user("4")).unwrap();
    let id5 = session.append_message(&assistant("5")).unwrap();
    let old_id = session.session_id().to_string();
    assert_eq!(session.create_branched_session(&id5).unwrap(), None);
    let all: Vec<&str> = session.entries().iter().map(|entry| entry.id()).collect();
    assert_eq!(all, [&id1, &id2, &id4, &id5]);
    assert_ne!(session.session_id(), old_id);
    assert_eq!(&session.session_id()[14..15], "7");
}

#[test]
fn labels_set_clear_and_resolve_in_the_tree() {
    let mut session = memory();
    let msg = session.append_message(&user("hello")).unwrap();
    assert_eq!(session.get_label(&msg), None);
    let label_id = session.append_label_change(&msg, Some("checkpoint")).unwrap();
    assert_eq!(session.get_label(&msg), Some("checkpoint"));
    assert!(matches!(session.get_entry(&label_id).unwrap().data(),
        EntryData::Label { target_id, label } if target_id == &msg && label.as_deref() == Some("checkpoint")));
    session.append_label_change(&msg, None).unwrap();
    assert_eq!(session.get_label(&msg), None);
    session.append_label_change(&msg, Some("first")).unwrap();
    let last = session.append_label_change(&msg, Some("third")).unwrap();
    assert_eq!(session.get_label(&msg), Some("third"));
    let tree = session.get_tree();
    let node = tree.node(tree.find(&msg).unwrap());
    assert_eq!(node.label.as_deref(), Some("third"));
    assert_eq!(node.label_timestamp.as_deref(), Some(session.get_entry(&last).unwrap().timestamp()));
    assert_eq!(roles(&session.build_session_context().messages), ["user"]);
    let error = session.append_label_change("non-existent", Some("label")).unwrap_err();
    assert_eq!(error.to_string(), "Entry non-existent not found");
}

#[test]
fn branched_sessions_rewrite_labels_on_the_path() {
    let mut session = memory();
    let msg1 = session.append_message(&user("hello")).unwrap();
    let msg2 = session.append_message(&assistant("hi")).unwrap();
    let msg3 = session.append_message(&user("followup")).unwrap();
    session.append_label_change(&msg1, Some("first")).unwrap();
    session.append_label_change(&msg2, Some("second")).unwrap();
    session.append_label_change(&msg3, Some("third")).unwrap();
    session.create_branched_session(&msg2).unwrap();
    assert_eq!(
        (session.get_label(&msg1), session.get_label(&msg2), session.get_label(&msg3)),
        (Some("first"), Some("second"), None)
    );
    assert_eq!(session.entries().iter().filter(|entry| entry.kind() == "label").count(), 2);

    let mut session = memory();
    let msg1 = session.append_message(&user("hello")).unwrap();
    session.append_label_change(&msg1, Some("checkpoint")).unwrap();
    let model_change = session.append_model_change("anthropic", "claude-test").unwrap();
    let msg2 = session.append_message(&user("followup")).unwrap();
    session.create_branched_session(&msg2).unwrap();
    assert_eq!(session.get_entry(&model_change).unwrap().parent_id(), Some(msg1.as_str()));
    assert_eq!(session.get_label(&msg1), Some("checkpoint"));
}

#[test]
fn session_names_sanitize_and_clear() {
    let mut session = memory();
    assert_eq!(session.session_name(), None);
    session.append_session_info("  Refactor\r\n\nauth  ").unwrap();
    assert_eq!(session.session_name().as_deref(), Some("Refactor auth"));
    session.append_session_info("   ").unwrap();
    assert_eq!(session.session_name(), None);
}
