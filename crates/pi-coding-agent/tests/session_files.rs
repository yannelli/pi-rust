//! Ported from upstream `test/session-manager/file-operations.test.ts`,
//! `custom-session-id.test.ts`, the persisted `createBranchedSession` tests, and
//! `session-file-invalid.test.ts`.

mod common;

use std::path::Path;

use common::*;
use pi_ai::Message;
use pi_coding_agent::session::{
    NewSessionOptions, SessionManager, find_most_recent_session, load_entries_from_file, session_dir,
};
use serde_json::json;

const HEADER: &str = r#"{"type":"session","id":"abc","timestamp":"2025-01-01T00:00:00Z","cwd":"/tmp"}"#;
const MESSAGE: &str = r#"{"type":"message","id":"1","parentId":null,"timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"hi","timestamp":1}}"#;

#[test]
fn loads_only_files_with_a_session_header() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, text: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    };
    assert!(load_entries_from_file(&dir.path().join("missing.jsonl")).is_empty());
    assert!(load_entries_from_file(&write("empty.jsonl", "")).is_empty());
    assert!(load_entries_from_file(&write("no-header.jsonl", "{\"type\":\"message\",\"id\":\"1\"}\n")).is_empty());
    assert!(load_entries_from_file(&write("bad.jsonl", "not json\n")).is_empty());
    assert_eq!(load_entries_from_file(&write("valid.jsonl", &format!("{HEADER}\n{MESSAGE}\n"))).len(), 2);
    assert_eq!(
        load_entries_from_file(&write("mixed.jsonl", &format!("{HEADER}\nnot valid json\n{MESSAGE}\n"))).len(),
        2
    );
    let unterminated = format!("{HEADER}\n{MESSAGE}");
    let path = write("unterminated.jsonl", &unterminated);
    assert_eq!(load_entries_from_file(&path).len(), 2);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{unterminated}\n"));
    let tail = format!("{HEADER}\n{{\"type\":\"message\"");
    let path = write("tail.jsonl", &tail);
    assert_eq!(load_entries_from_file(&path).len(), 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{tail}\n"));
    let invalid = "{\"type\":\"message\",\"id\":\"1\"}";
    let path = write("invalid.jsonl", invalid);
    assert!(load_entries_from_file(&path).is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
}

#[test]
fn opens_headers_after_noise_and_beyond_the_scan_limit() {
    let dir = tempfile::tempdir().unwrap();
    let stored = dir.path().join("stored-project");
    for (name, prefix, id) in [
        ("blank", "\n  \n".to_string(), "leading-blank".to_string()),
        ("malformed", "not json\n{broken json\n".to_string(), "leading-malformed".to_string()),
        ("large-header", String::new(), "a".repeat(1024 * 1024 + 1)),
        ("large-prefix", format!("{}\n", "x".repeat(1024 * 1024 + 1)), "large-prefix".to_string()),
    ] {
        let path = dir.path().join(format!("{name}.jsonl"));
        let header =
            json!({"type": "session", "version": 3, "id": id, "timestamp": "2025-01-01T00:00:00Z", "cwd": stored});
        std::fs::write(&path, format!("{prefix}{header}\n")).unwrap();
        let session = SessionManager::open_with(&path, Some(dir.path()), None).unwrap();
        assert_eq!((session.session_id(), session.cwd()), (id.as_str(), stored.as_path()));
        let other = dir.path().join("override");
        let session = SessionManager::open_with(&path, Some(dir.path()), Some(&other)).unwrap();
        assert_eq!(session.cwd(), other);
    }
}

#[test]
fn finds_the_most_recent_valid_session() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(find_most_recent_session(dir.path(), None), None);
    assert_eq!(find_most_recent_session(&dir.path().join("missing"), None), None);
    std::fs::write(dir.path().join("file.txt"), "hello").unwrap();
    std::fs::write(dir.path().join("invalid.jsonl"), "{\"type\":\"message\"}\n").unwrap();
    std::fs::write(dir.path().join("oversized.jsonl"), "x".repeat(1024 * 1024 + 1)).unwrap();
    assert_eq!(find_most_recent_session(dir.path(), None), None);
    let header =
        |id: &str, cwd: &Path| json!({"type": "session", "id": id, "timestamp": "2025-01-01T00:00:00Z", "cwd": cwd});
    let (project_a, project_b) = (dir.path().join("project-a"), dir.path().join("project-b"));
    let file_a = dir.path().join("a.jsonl");
    std::fs::write(&file_a, format!("{}\n", header("a", &project_a))).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let file_b = dir.path().join("b.jsonl");
    std::fs::write(&file_b, format!("{}\n", header("b", &project_b))).unwrap();
    assert_eq!(find_most_recent_session(dir.path(), None), Some(file_b.clone()));
    assert_eq!(find_most_recent_session(dir.path(), Some(&project_a)), Some(file_a));
    assert_eq!(find_most_recent_session(dir.path(), Some(&project_b)), Some(file_b));
}

#[test]
fn rejects_or_repairs_unusable_session_files() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    let session = SessionManager::open_with(&empty, Some(dir.path()), None).unwrap();
    assert_eq!(session.session_file(), Some(empty.as_path()));
    let lines = file_roles(&empty);
    assert_eq!(lines, ["session"]);
    let reopened = SessionManager::open_with(&empty, Some(dir.path()), None).unwrap();
    assert_eq!(reopened.session_id(), session.session_id());
    for (name, content) in [
        (
            "no-header.jsonl",
            "{\"type\":\"message\",\"id\":\"abc\",\"parentId\":\"orphaned\",\"timestamp\":\"2025-01-01T00:00:00Z\",\"message\":{\"role\":\"assistant\",\"content\":\"test\"}}\n",
        ),
        ("not-a-session.log", "{\"type\":\"event\",\"data\":\"not a session\"}\n"),
    ] {
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let error = SessionManager::open_with(&path, Some(dir.path()), None).unwrap_err().to_string();
        assert_eq!(error, format!("Session file is not a valid pi session: {}", path.display()));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }
    let missing = dir.path().join("new.jsonl");
    let mut fresh = SessionManager::open(&missing).unwrap();
    assert!(!missing.exists());
    fresh.append_message(&user("hi")).unwrap();
    assert_eq!(file_roles(&missing), ["session", "user"]);
}

#[test]
fn writes_lazily_from_the_first_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), Some(dir.path()));
    session.append_model_change("anthropic", "claude-sonnet-4-5").unwrap();
    session.append_thinking_level_change(pi_ai::ThinkingLevel::Off).unwrap();
    let file = session.session_file().unwrap().to_path_buf();
    assert!(!file.exists(), "setup entries alone leave no file");
    session.append_message(&user("first question")).unwrap();
    assert_eq!(file_roles(&file), ["session", "model_change", "thinking_level_change", "user"]);
    session.append_custom_entry("preset-state", Some(json!({"name": "plan"}))).unwrap();
    session.append_message(&assistant("first answer")).unwrap();
    assert_eq!(file_roles(&file), ["session", "model_change", "thinking_level_change", "user", "custom", "assistant"]);
    let reopened = SessionManager::open_with(&file, Some(dir.path()), None).unwrap();
    assert_eq!(reopened.build_session_context().messages.len(), 2);
    assert_eq!(raw_entries(&reopened), raw_entries(&session));
}

#[test]
fn custom_session_ids_are_validated_and_used_for_files() {
    let mut session = memory();
    session.new_session(NewSessionOptions { id: Some("abc-123_def.456".into()), parent_session: None }).unwrap();
    assert_eq!((session.session_id(), session.header().id.as_str()), ("abc-123_def.456", "abc-123_def.456"));
    for id in ["", "-abc", "abc-", "_abc", "abc_", ".abc", "abc.", "abc/def", "abc\\def", "abc def"] {
        let error = memory().new_session(NewSessionOptions { id: Some(id.into()), parent_session: None }).unwrap_err();
        assert!(error.to_string().starts_with("Session id must be non-empty, contain only alphanumeric characters"));
    }
    session.new_session(NewSessionOptions { id: None, parent_session: Some("parent.jsonl".into()) }).unwrap();
    assert_eq!(&session.session_id()[14..15], "7");
    assert_eq!(session.header().parent_session.as_deref(), Some("parent.jsonl"));

    let dir = tempfile::tempdir().unwrap();
    let options = NewSessionOptions { id: Some("created-session-id".into()), parent_session: None };
    let created = SessionManager::create_with(dir.path(), Some(dir.path()), options).unwrap();
    let name = created.session_file().unwrap().file_name().unwrap().to_string_lossy().to_string();
    assert!(
        name.ends_with("Z_created-session-id.jsonl")
            && name.len() == "2025-01-01T00-00-00-000Z_created-session-id.jsonl".len()
    );
    assert!(!created.session_file().unwrap().exists());
}

#[test]
fn fork_from_copies_entries_with_a_parent_link() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.jsonl");
    let header = json!({"type": "session", "version": 3, "id": "legacy-session-id", "timestamp": "2025-01-01T00:00:00Z", "cwd": dir.path()});
    let entry = message_entry("entry-1", None, &assistant("hello"));
    std::fs::write(&source, format!("{header}\n{entry}\n")).unwrap();
    let agent = dir.path().join("agent");
    let forked =
        SessionManager::fork_from(&source, dir.path(), Some(dir.path()), &agent, NewSessionOptions::default()).unwrap();
    let header = forked.header();
    assert_eq!(&header.id[14..15], "7");
    assert_eq!(header.parent_session.as_deref(), Some(source.to_str().unwrap()));
    assert_eq!(
        forked.entries()[0].raw(),
        &serde_json::from_value::<pi_coding_agent::session::FileEntry>(entry).unwrap()
    );
    let options = NewSessionOptions { id: Some("forked-session-id".into()), parent_session: None };
    let named = SessionManager::fork_from(&source, dir.path(), Some(dir.path()), &agent, options).unwrap();
    assert!(named.session_file().unwrap().to_string_lossy().ends_with("_forked-session-id.jsonl"));
    assert_eq!(named.header().id, "forked-session-id");
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    let error = SessionManager::fork_from(&empty, dir.path(), None, &agent, NewSessionOptions::default()).unwrap_err();
    assert!(error.to_string().starts_with("Cannot fork: source session file is empty or invalid"));
    // Without a directory, the fork lands in the target's default session directory.
    let default = SessionManager::fork_from(&source, dir.path(), None, &agent, NewSessionOptions::default()).unwrap();
    assert_eq!(default.session_dir(), session_dir(&agent, dir.path()));
}

#[test]
fn branched_session_files_are_written_once_a_conversation_exists() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), Some(dir.path()));
    let model_change = session.append_model_change("anthropic", "claude-sonnet-4-5").unwrap();
    session.append_message(&user("first question")).unwrap();
    session.append_message(&assistant("first answer")).unwrap();
    let source = session.session_file().unwrap().to_path_buf();
    let file = session.create_branched_session(&model_change).unwrap().unwrap();
    assert!(!file.exists());
    assert_eq!(session.header().parent_session.as_deref(), Some(source.to_str().unwrap()));
    session.append_message(&user("new question")).unwrap();
    session.append_custom_entry("preset-state", Some(json!({"name": "plan"}))).unwrap();
    session.append_message(&assistant("new answer")).unwrap();
    assert_eq!(file_roles(&file), ["session", "model_change", "user", "custom", "assistant"]);

    let mut session = SessionManager::create(dir.path(), Some(dir.path()));
    let first = session.append_message(&user("first question")).unwrap();
    session.append_message(&assistant("first answer")).unwrap();
    let file = session.create_branched_session(&first).unwrap().unwrap();
    assert!(file.exists());
    session.append_message(&assistant("new answer")).unwrap();
    assert_eq!(file_roles(&file), ["session", "user", "assistant"]);
}

#[test]
fn tool_and_summary_usage_survive_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), Some(dir.path()));
    let root = session.append_message(&user("question")).unwrap();
    session.append_message(&assistant("answer")).unwrap();
    let summary_usage = usage(10, 20, 30, 40);
    let result = pi_ai::ToolResultMessage {
        tool_call_id: "call-1".into(),
        tool_name: "nested-model".into(),
        content: vec![pi_ai::ContentBlock::text("result")],
        usage: Some(summary_usage),
        ..Default::default()
    };
    session.append_message(&Message::ToolResult(result)).unwrap();
    session
        .append_compaction_with(pi_coding_agent::session::NewCompaction {
            summary: "summary".into(),
            first_kept_entry_id: Some(root.clone()),
            tokens_before: 100,
            from_hook: Some(false),
            usage: Some(summary_usage),
            ..Default::default()
        })
        .unwrap();
    session.branch_with_summary(Some(&root), "branch summary", None, Some(false), Some(summary_usage)).unwrap();
    let reopened = SessionManager::open_with(session.session_file().unwrap(), Some(dir.path()), None).unwrap();
    let entries = reopened.entries();
    assert_eq!(entries[3].compaction().unwrap().usage, Some(summary_usage));
    assert_eq!(entries[4].branch_summary().unwrap().usage, Some(summary_usage));
    assert!(matches!(entries[2].message(), Some(Message::ToolResult(result)) if result.usage == Some(summary_usage)));
    let stats = reopened.session_stats();
    assert_eq!((stats.user_messages, stats.assistant_messages, stats.tool_results, stats.total_messages), (1, 1, 1, 3));
    assert_eq!(stats.tokens.input, 1 + 10 * 3);
}
