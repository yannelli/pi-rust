//! Upstream-format files: v1 and v2 migration (upstream `migration.test.ts`), a v3 file with
//! every entry type and role that must round-trip byte for byte, and JSONL export and import.

mod common;

use common::*;
use pi_coding_agent::messages::convert_to_llm;
use pi_coding_agent::session::branching::import_session_file;
use pi_coding_agent::session::{EntryData, SessionManager, migrate_session_entries, parse_session_entries};
use serde_json::Value;

const ALL_ENTRY_TYPES: &[&str] = &[
    r#"{"type":"session","version":3,"id":"0193b5e2-7c1a-7d4e-9f00-123456789abc","timestamp":"2024-12-03T14:00:00.000Z","cwd":"/project","parentSession":"/sessions/parent.jsonl"}"#,
    r#"{"type":"message","id":"a0b1c2d3","parentId":null,"timestamp":"2024-12-03T14:00:00.000Z","message":{"role":"system","content":"","sections":{"preamble":"You are an expert coding assistant.","cwd":"/project"},"toolsAdded":[{"name":"read","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],"timestamp":1733234400000}}"#,
    r#"{"type":"model_change","id":"b1c2d3e4","parentId":"a0b1c2d3","timestamp":"2024-12-03T14:00:00.500Z","provider":"anthropic","modelId":"claude-sonnet-4-5"}"#,
    r#"{"type":"thinking_level_change","id":"c2d3e4f5","parentId":"b1c2d3e4","timestamp":"2024-12-03T14:00:00.600Z","thinkingLevel":"high"}"#,
    r#"{"type":"message","id":"d3e4f5a6","parentId":"c2d3e4f5","timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"Hello","timestamp":1733234401000}}"#,
    r#"{"type":"message","id":"e4f5a6b7","parentId":"d3e4f5a6","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"Plan the read.","thinkingSignature":"sig=="},{"type":"text","text":"Reading it."},{"type":"toolCall","id":"call_123","name":"read","arguments":{"path":"src/main.ts"}}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","responseModel":"claude-sonnet-4-5-20250929","thinkingLevel":"high","usage":{"input":1200,"output":80,"cacheRead":300,"cacheWrite":50,"cacheWrite1h":20,"totalTokens":1630,"cost":{"input":0.0036,"output":0.0012,"cacheRead":0.00009,"cacheWrite":0.0001875,"total":0.0050775}},"stopReason":"toolUse","timestamp":1733234402000,"durationMs":950}}"#,
    r#"{"type":"message","id":"f5a6b7c8","parentId":"e4f5a6b7","timestamp":"2024-12-03T14:00:03.000Z","message":{"role":"toolResult","toolCallId":"call_123","toolName":"read","content":[{"type":"text","text":"console.log(1)"},{"type":"image","data":"iVBORw0KGgo=","mimeType":"image/png"}],"details":{"path":"src/main.ts","lines":1},"isError":false,"timestamp":1733234403000}}"#,
    r#"{"type":"message","id":"a6b7c8d9","parentId":"f5a6b7c8","timestamp":"2024-12-03T14:00:04.000Z","message":{"role":"bashExecution","command":"ls -la","output":"total 0","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1733234404000}}"#,
    r#"{"type":"message","id":"b7c8d9e0","parentId":"a6b7c8d9","timestamp":"2024-12-03T14:00:05.000Z","message":{"role":"bashExecution","command":"cat .env","output":"SECRET=1","exitCode":0,"cancelled":false,"truncated":false,"timestamp":1733234405000,"excludeFromContext":true}}"#,
    r#"{"type":"message","id":"c8d9e0f1","parentId":"b7c8d9e0","timestamp":"2024-12-03T14:00:06.000Z","message":{"role":"custom","customType":"reminder","content":"Remember the tests.","display":true,"timestamp":1733234406000}}"#,
    r#"{"type":"usage","id":"d9e0f1a2","parentId":"c8d9e0f1","timestamp":"2024-12-03T14:00:07.000Z","kind":"cache_warm","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":0,"output":0,"cacheRead":50000,"cacheWrite":0,"totalTokens":50000,"cost":{"input":0,"output":0,"cacheRead":0.015,"cacheWrite":0,"total":0.015}},"note":"warm"}"#,
    r#"{"type":"custom","customType":"my-extension","data":{"count":42},"id":"e0f1a2b3","parentId":"d9e0f1a2","timestamp":"2024-12-03T14:00:08.000Z"}"#,
    r#"{"type":"custom_message","customType":"my-extension","content":[{"type":"text","text":"Injected context..."}],"display":false,"details":{"source":"hook"},"id":"f1a2b3c4","parentId":"e0f1a2b3","timestamp":"2024-12-03T14:00:09.000Z"}"#,
    r#"{"type":"compaction","id":"a2b3c4d5","parentId":"f1a2b3c4","timestamp":"2024-12-03T14:10:00.000Z","summary":"User discussed X, Y, Z...","firstKeptEntryId":"d3e4f5a6","tokensBefore":50000,"details":{"readFiles":["src/main.ts"],"modifiedFiles":[]},"usage":{"input":4000,"output":500,"cacheRead":0,"cacheWrite":0,"totalTokens":4500,"cost":{"input":0.012,"output":0.0075,"cacheRead":0,"cacheWrite":0,"total":0.0195}},"fromHook":false,"systemMessage":{"role":"system","content":"","sections":{"preamble":"You are an expert coding assistant.","cwd":"/project"},"toolsAdded":[{"name":"read","description":"Read a file","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}],"timestamp":1733235000000}}"#,
    r#"{"type":"context_edit","id":"b3c4d5e6","parentId":"a2b3c4d5","timestamp":"2024-12-03T14:11:00.000Z","targetId":"c8d9e0f1","replacement":{"content":"Remember the lint step."}}"#,
    r#"{"type":"context_edit","id":"c4d5e6f7","parentId":"b3c4d5e6","timestamp":"2024-12-03T14:11:01.000Z","targetId":"f1a2b3c4","replacement":null}"#,
    r#"{"type":"label","id":"d5e6f7a8","parentId":"c4d5e6f7","timestamp":"2024-12-03T14:12:00.000Z","targetId":"d3e4f5a6","label":"checkpoint-1"}"#,
    r#"{"type":"label","id":"e6f7a8b9","parentId":"d5e6f7a8","timestamp":"2024-12-03T14:12:30.000Z","targetId":"e4f5a6b7"}"#,
    r#"{"type":"branch_summary","id":"f7a8b9c0","parentId":"e6f7a8b9","timestamp":"2024-12-03T14:15:00.000Z","fromId":"e4f5a6b7","summary":"Branch explored approach A...","details":{"readFiles":[],"modifiedFiles":["a.ts"]},"usage":{"input":100,"output":10,"cacheRead":0,"cacheWrite":0,"totalTokens":110,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"fromHook":true}"#,
    r#"{"type":"session_info","id":"a8b9c0d1","parentId":"f7a8b9c0","timestamp":"2024-12-03T14:35:00.000Z","name":"Refactor auth module"}"#,
    r#"{"type":"future_entry_type","id":"b9c0d1e2","parentId":"a8b9c0d1","timestamp":"2024-12-03T14:36:00.000Z","payload":{"kept":true}}"#,
    r#"{"type":"message","id":"c0d1e2f3","parentId":"b9c0d1e2","timestamp":"2024-12-03T14:40:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Continue"}],"timestamp":1733237200000}}"#,
];

#[test]
fn every_entry_type_round_trips_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("all.jsonl");
    let original: String = ALL_ENTRY_TYPES.iter().map(|line| format!("{line}\n")).collect();
    std::fs::write(&path, &original).unwrap();
    let mut session = SessionManager::open(&path).unwrap();
    assert_eq!(session.to_jsonl(), original);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "opening a v3 file never rewrites it");
    let typed: Vec<&str> =
        session.entries().iter().filter(|entry| matches!(entry.data(), EntryData::Other)).map(|e| e.kind()).collect();
    assert_eq!(typed, ["future_entry_type"]);
    assert_eq!(session.session_name().as_deref(), Some("Refactor auth module"));
    assert_eq!(session.get_label("d3e4f5a6"), Some("checkpoint-1"));
    assert_eq!(session.get_label("e4f5a6b7"), None);
    assert_eq!(session.header().parent_session.as_deref(), Some("/sessions/parent.jsonl"));

    let context = session.build_session_context();
    let expected = [
        "system",
        "compactionSummary",
        "user",
        "assistant",
        "toolResult",
        "bashExecution",
        "bashExecution",
        "custom",
        "branchSummary",
        "user",
    ];
    assert_eq!(roles(&context.messages), expected);
    assert_eq!(context.model, Some(("anthropic".into(), "claude-sonnet-4-5".into())));
    let llm = convert_to_llm(&context.messages);
    assert_eq!(roles(&llm), ["system", "user", "user", "assistant", "toolResult", "user", "user", "user", "user"]);
    assert_eq!(text(&llm[6]), "Remember the lint step.");
    let stats = session.session_stats();
    assert_eq!(stats.tokens.cache_read, 50_300);
    assert!((stats.cost - (0.0050775 + 0.015 + 0.0195)).abs() < 1e-12);

    session.append_message(&user("appended")).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with(&original) && written.lines().count() == ALL_ENTRY_TYPES.len() + 1);
    let copy = SessionManager::fork_from(&path, dir.path(), Some(dir.path()), dir.path(), Default::default()).unwrap();
    let copied: String = copy.to_jsonl().lines().skip(1).map(|line| format!("{line}\n")).collect();
    assert_eq!(copied, written.lines().skip(1).map(|line| format!("{line}\n")).collect::<String>());
}

const V1_SESSION: &[&str] = &[
    r#"{"type":"session","id":"d703a1a9-1b7b-4fb1-b512-c9738b1fe617","timestamp":"2025-11-20T23:33:50.805Z","cwd":"/work","provider":"anthropic","modelId":"claude-sonnet-4-5","thinkingLevel":"off"}"#,
    r#"{"type":"message","timestamp":"2025-11-20T23:33:51.000Z","message":{"role":"user","content":[{"type":"text","text":"first"}],"timestamp":1763681631000}}"#,
    r#"{"type":"message","timestamp":"2025-11-20T23:33:52.000Z","message":{"role":"assistant","content":[{"type":"text","text":"answer one"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1763681632000}}"#,
    r#"{"type":"message","timestamp":"2025-11-20T23:33:53.000Z","message":{"role":"user","content":"second","timestamp":1763681633000}}"#,
    r#"{"type":"thinking_level_change","timestamp":"2025-11-20T23:33:54.000Z","thinkingLevel":"medium"}"#,
    r#"{"type":"message","timestamp":"2025-11-20T23:33:55.000Z","message":{"role":"assistant","content":[{"type":"text","text":"answer two"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-4-5","usage":{"input":2,"output":2,"cacheRead":0,"cacheWrite":0},"stopReason":"stop","timestamp":1763681635000}}"#,
    r#"{"type":"compaction","timestamp":"2025-11-20T23:34:00.000Z","summary":"Earlier work","firstKeptEntryIndex":3,"tokensBefore":1234}"#,
    r#"{"type":"message","timestamp":"2025-11-20T23:34:01.000Z","message":{"role":"user","content":"third","timestamp":1763681641000}}"#,
];

#[test]
fn v1_files_migrate_to_v3_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.jsonl");
    std::fs::write(&path, V1_SESSION.join("\n") + "\n").unwrap();
    let session = SessionManager::open(&path).unwrap();
    let rewritten: Vec<Value> =
        std::fs::read_to_string(&path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let header = rewritten[0].as_object().unwrap();
    assert_eq!(header.keys().next_back().map(String::as_str), Some("version"));
    assert_eq!(header["version"], 3);
    assert_eq!(header["provider"], "anthropic");
    let entries = session.entries();
    assert!(entries.iter().all(|entry| entry.id().len() == 8));
    assert_eq!(entries[0].parent_id(), None);
    for pair in entries.windows(2) {
        assert_eq!(pair[1].parent_id(), Some(pair[0].id()));
    }
    let compaction = rewritten[6].as_object().unwrap();
    assert!(!compaction.contains_key("firstKeptEntryIndex"));
    assert_eq!(compaction["firstKeptEntryId"], entries[2].id());
    let keys: Vec<&str> = rewritten[1].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["type", "timestamp", "message", "id", "parentId"]);
    let context = session.build_session_context();
    assert_eq!(
        context.messages.iter().map(text).collect::<Vec<_>>(),
        ["Earlier work", "second", "answer two", "third"]
    );
    assert_eq!(context.thinking_level, Some(pi_ai::ThinkingLevel::Medium));
}

#[test]
fn v2_files_rename_hook_messages_and_keep_ids() {
    let lines = [
        r#"{"type":"session","version":2,"id":"sess-2","timestamp":"2025-01-01T00:00:00Z","cwd":"/tmp"}"#,
        r#"{"type":"message","id":"abc12345","parentId":null,"timestamp":"2025-01-01T00:00:01Z","message":{"role":"user","content":"hi","timestamp":1}}"#,
        r#"{"type":"message","id":"def67890","parentId":"abc12345","timestamp":"2025-01-01T00:00:02Z","message":{"role":"hookMessage","customType":"hook","content":"from a hook","display":true,"timestamp":2}}"#,
    ];
    let mut entries = parse_session_entries(&lines.join("\n"));
    assert!(migrate_session_entries(&mut entries));
    assert_eq!(entries[0]["version"], 3);
    assert_eq!((entries[1]["id"].as_str(), entries[2]["parentId"].as_str()), (Some("abc12345"), Some("abc12345")));
    assert_eq!(entries[2]["message"]["role"], "custom");
    assert!(!migrate_session_entries(&mut entries), "migration is idempotent");

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    let session = SessionManager::open(&path).unwrap();
    assert_eq!(roles(&session.build_session_context().messages), ["user", "custom"]);
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains(r#""role":"custom""#) && stored.starts_with(r#"{"type":"session","version":3,"#));
}

#[test]
fn exports_the_branch_and_imports_jsonl_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = SessionManager::create(dir.path(), Some(&dir.path().join("sessions")));
    let first = session.append_message(&user("first")).unwrap();
    session.append_message(&assistant("abandoned")).unwrap();
    session.branch(&first).unwrap();
    session.append_message(&assistant("kept")).unwrap();
    let exported = session.serialize_branch(None);
    let lines: Vec<Value> = exported.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!((lines[0]["type"].as_str(), lines[0]["id"].as_str()), (Some("session"), Some(session.session_id())));
    assert_eq!(lines[1]["parentId"], Value::Null);
    assert_eq!(lines[2]["parentId"], lines[1]["id"]);
    let output = session.export_branch_to_jsonl(Some(&dir.path().join("out/export.jsonl"))).unwrap();
    assert!(output.exists());

    let store = dir.path().join("store");
    let imported = import_session_file(&output, &store, None).unwrap();
    assert_eq!(imported.session_file(), Some(store.join("export.jsonl").as_path()));
    assert_eq!(roles(&imported.build_session_context().messages), ["user", "assistant"]);
    let again = import_session_file(&output, &store, None).unwrap();
    assert_eq!(again.session_file(), Some(store.join("export-1.jsonl").as_path()));
    let in_place = import_session_file(&store.join("export.jsonl"), &store, Some(dir.path())).unwrap();
    assert_eq!(in_place.session_file(), Some(store.join("export.jsonl").as_path()));
    assert_eq!(in_place.cwd(), dir.path());
    assert!(import_session_file(&dir.path().join("missing.jsonl"), &store, None).is_err());
}
