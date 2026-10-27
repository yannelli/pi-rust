//! Session discovery, ported from upstream `file-operations.test.ts` (flat directories), the
//! `SessionInfo` timestamp test, `session-selector-search.test.ts`, and the `--session` id
//! resolution in `main.ts`.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use pi_coding_agent::session::listing::{
    DeleteMethod, ResolvedSession, delete_session_file, list_all_sessions, list_project_sessions, rename_session,
    resolve_session_arg,
};
use pi_coding_agent::session::search::{NameFilter, SortMode, filter_and_sort_sessions, threaded_sessions};
use pi_coding_agent::session::usage::usage_cost_breakdown;
use pi_coding_agent::session::{NewSessionOptions, SessionInfo, SessionManager, list_sessions, parse_iso, session_dir};

fn persisted(cwd: &Path, dir: &Path, label: &str, id: Option<&str>) -> PathBuf {
    let options = NewSessionOptions { id: id.map(String::from), parent_session: None };
    let mut session = SessionManager::create_with(cwd, Some(dir), options).unwrap();
    session.append_message(&user(label)).unwrap();
    session.append_message(&assistant(&format!("reply to {label}"))).unwrap();
    session.session_file().unwrap().to_path_buf()
}

fn paths(sessions: Vec<SessionInfo>) -> Vec<PathBuf> {
    sessions.into_iter().map(|session| session.path).collect()
}

#[test]
fn flat_directories_scope_project_listing_by_cwd() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let (project_a, project_b, flat) = (root.path().join("a"), root.path().join("b"), root.path().join("flat"));
    let session_a = persisted(&project_a, &flat, "from A", None);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let session_b = persisted(&project_b, &flat, "from B", None);
    assert_eq!(paths(list_project_sessions(&project_a, Some(&flat), &agent)), std::slice::from_ref(&session_a));
    assert_eq!(paths(list_all_sessions(Some(&flat), &agent)), [session_b, session_a.clone()]);
    let continued = SessionManager::continue_recent(&project_a, &flat, &agent).unwrap();
    assert_eq!(continued.session_file(), Some(session_a.as_path()));
    let fresh = SessionManager::continue_recent(&root.path().join("c"), &flat, &agent).unwrap();
    assert_ne!(fresh.session_file(), Some(session_a.as_path()));
}

#[test]
fn lists_all_projects_under_the_agent_directory() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let (project_a, project_b) = (root.path().join("a"), root.path().join("b"));
    let session_a = persisted(&project_a, &session_dir(&agent, &project_a), "alpha", None);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let session_b = persisted(&project_b, &session_dir(&agent, &project_b), "beta", None);
    assert_eq!(paths(list_all_sessions(None, &agent)), [session_b, session_a]);
    let local = list_project_sessions(&project_a, None, &agent);
    assert_eq!(local.len(), 1);
    let info = &local[0];
    assert_eq!((info.first_message.as_str(), info.message_count), ("alpha", 2));
    assert_eq!(info.cwd, project_a.display().to_string());
    assert_eq!(info.all_messages_text, "alpha reply to alpha");
    assert_eq!(info.display_name(), "alpha");
    assert_eq!(list_sessions(&session_dir(&agent, &project_a)).len(), 1);
}

#[test]
fn activity_time_comes_from_the_last_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let header =
        r#"{"type":"session","id":"test-session","version":3,"timestamp":"1970-01-01T00:00:00.000Z","cwd":"/tmp"}"#;
    std::fs::write(&path, format!("{header}\n")).unwrap();
    let mut session = SessionManager::open(&path).unwrap();
    let mut late = assistant("later");
    if let pi_ai::Message::Assistant(message) = &mut late {
        message.timestamp = 1_800_000_000_000;
    }
    session.append_message(&late).unwrap();
    session.append_session_info("Named session").unwrap();
    let info = list_project_sessions(Path::new("/tmp"), Some(dir.path()), dir.path()).remove(0);
    assert_eq!(info.modified, 1_800_000_000_000);
    assert_eq!((info.created, info.first_message.as_str()), (0, "(no messages)"));
    assert_eq!(info.display_name(), "Named session");
}

#[test]
fn resolves_paths_and_exact_or_partial_ids() {
    let root = tempfile::tempdir().unwrap();
    let agent = root.path().join("agent");
    let (here, elsewhere) = (root.path().join("here"), root.path().join("elsewhere"));
    let local = persisted(&here, &session_dir(&agent, &here), "local", Some("aaaa1111-local"));
    let remote = persisted(&elsewhere, &session_dir(&agent, &elsewhere), "remote", Some("bbbb2222-remote"));
    let resolve = |arg: &str| resolve_session_arg(arg, &here, None, &agent);
    assert_eq!(resolve("aaaa1111-local"), ResolvedSession::Local(local.clone()));
    assert_eq!(resolve("aaaa"), ResolvedSession::Local(local.clone()));
    assert_eq!(resolve("bbbb"), ResolvedSession::Global { path: remote, cwd: elsewhere.display().to_string() });
    assert_eq!(resolve("cccc"), ResolvedSession::NotFound("cccc".into()));
    assert_eq!(resolve("dir/file.jsonl"), ResolvedSession::Path(here.join("dir/file.jsonl")));
    assert_eq!(SessionManager::find_by_id(&here, "aaaa1111-local", None, &agent), Some(local));
    assert_eq!(SessionManager::find_by_id(&here, "aaaa", None, &agent), None);
}

#[test]
fn renames_and_deletes_session_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = persisted(dir.path(), dir.path(), "to rename", None);
    rename_session(&path, "  ").unwrap();
    assert_eq!(list_sessions(dir.path())[0].name, None);
    rename_session(&path, "Better name").unwrap();
    assert_eq!(list_sessions(dir.path())[0].name.as_deref(), Some("Better name"));
    let method = delete_session_file(&path).unwrap();
    assert!(matches!(method, DeleteMethod::Trash | DeleteMethod::Unlink));
    assert!(!path.exists());
    let missing = delete_session_file(&dir.path().join("-missing.jsonl")).unwrap();
    assert_eq!(missing, DeleteMethod::Trash);
}

fn info(id: &str, modified: &str, text: &str, name: Option<&str>) -> SessionInfo {
    let activity = parse_iso(modified).unwrap();
    SessionInfo {
        path: PathBuf::from(format!("/tmp/{id}.jsonl")),
        id: id.into(),
        cwd: String::new(),
        name: name.map(String::from),
        parent_session_path: None,
        created: 0,
        modified: activity,
        changed: std::time::UNIX_EPOCH + std::time::Duration::from_millis(activity as u64),
        message_count: 1,
        first_message: "(no messages)".into(),
        all_messages_text: text.into(),
    }
}

fn found(sessions: &[SessionInfo], query: &str, sort: SortMode, filter: NameFilter) -> Vec<String> {
    filter_and_sort_sessions(sessions, query, sort, filter).into_iter().map(|session| session.id).collect()
}

#[test]
fn search_supports_phrases_regex_and_sorting() {
    let phrase = [
        info("a", "2026-01-01T00:00:00Z", "node\n\n   cve was discussed", None),
        info("b", "2026-01-02T00:00:00Z", "node something else", None),
    ];
    assert_eq!(found(&phrase, "\"node cve\"", SortMode::Recent, NameFilter::All), ["a"]);
    let regex = [
        info("a", "2026-01-02T00:00:00Z", "Brave is great", None),
        info("b", "2026-01-03T00:00:00Z", "bravery is not the same", None),
    ];
    assert_eq!(found(&regex, "re:\\bbrave\\b", SortMode::Recent, NameFilter::All), ["a"]);
    assert!(found(&regex, "re:(", SortMode::Recent, NameFilter::All).is_empty());
    let recent = [
        info("newer", "2026-01-03T00:00:00Z", "brave", None),
        info("older", "2026-01-01T00:00:00Z", "brave", None),
        info("nomatch", "2026-01-04T00:00:00Z", "something else", None),
    ];
    assert_eq!(found(&recent, "\"brave\"", SortMode::Recent, NameFilter::All), ["newer", "older"]);
    // Name text ranks above message text; the position of a match does not count.
    let relevance = [
        info("message", "2026-01-03T00:00:00Z", "brave", None),
        info("name", "2026-01-01T00:00:00Z", "xxxx", Some("Brave work")),
    ];
    assert_eq!(found(&relevance, "\"brave\"", SortMode::Relevance, NameFilter::All), ["name", "message"]);
    assert_eq!(found(&recent[..2], "\"brave\"", SortMode::Relevance, NameFilter::All), ["newer", "older"]);
    // Scattered characters match the name or cwd, never message text.
    assert!(found(&recent, "brv", SortMode::Relevance, NameFilter::All).is_empty());
    assert_eq!(found(&relevance, "brv", SortMode::Relevance, NameFilter::All), ["name"]);
}

#[test]
fn named_filter_applies_before_the_query() {
    let sessions = [
        info("named1", "2026-01-03T00:00:00Z", "blueberry", Some("My Project")),
        info("named2", "2026-01-02T00:00:00Z", "blueberry", Some("Another Named")),
        info("other1", "2026-01-04T00:00:00Z", "blueberry", None),
        info("whitespace", "2026-01-01T00:00:00Z", "blueberry", Some("   ")),
    ];
    assert_eq!(found(&sessions, "", SortMode::Recent, NameFilter::All).len(), 4);
    assert_eq!(found(&sessions, "", SortMode::Recent, NameFilter::Named), ["named1", "named2"]);
    assert_eq!(found(&sessions, "blueberry", SortMode::Recent, NameFilter::Named), ["named1", "named2"]);
}

#[test]
fn threaded_order_nests_forks_under_parents() {
    let parent = info("parent", "2026-01-01T00:00:00Z", "", None);
    let mut child = info("child", "2026-01-05T00:00:00Z", "", None);
    child.parent_session_path = Some("/tmp/parent.jsonl".into());
    let other = info("other", "2026-01-03T00:00:00Z", "", None);
    let rows = threaded_sessions(&[other, child, parent]);
    let order: Vec<(&str, usize)> = rows.iter().map(|row| (row.session.id.as_str(), row.depth)).collect();
    assert_eq!(order, [("parent", 0), ("child", 1), ("other", 0)]);
}

#[test]
fn usage_breakdown_groups_models_and_summaries() {
    let mut session = memory();
    session.append_message(&assistant_with("a", usage(10, 5, 0, 0))).unwrap();
    session.append_usage("cache_warm", "anthropic", "warm-model", &usage(0, 0, 100, 0), Some("note")).unwrap();
    session.branch_with_summary(None, "summary", None, Some(false), Some(usage(1, 1, 0, 0))).unwrap();
    let breakdown = usage_cost_breakdown(session.entries());
    let keys: Vec<(&str, u64)> = breakdown.iter().map(|entry| (entry.key.as_str(), entry.tokens)).collect();
    assert_eq!(keys.len(), 3);
    assert!(keys.contains(&("anthropic/test", 15)) && keys.contains(&("anthropic/warm-model", 100)));
    assert!(keys.contains(&("Tools/summaries", 2)));
    assert_eq!(session.session_stats().tokens.tokens(), 117);
}
