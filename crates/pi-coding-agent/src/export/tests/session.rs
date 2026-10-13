use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::export::ExportTool;
use crate::export::jsonl::{export_session_to_jsonl, serialize_at};
use crate::export::session_file::{SessionSnapshot, iso_timestamp};
use crate::export::share::{create_share_trailing_entries, export_session_for_share};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/export_session.jsonl")
}

fn records(text: &str) -> Vec<Value> {
    assert!(text.ends_with('\n'));
    text.trim_end().split('\n').map(|line| serde_json::from_str(line).unwrap()).collect()
}

#[test]
fn snapshot_reads_header_entries_leaf_and_branch() {
    let snapshot = SessionSnapshot::load(&fixture()).unwrap();
    assert_eq!(snapshot.session_id(), "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b");
    assert_eq!(snapshot.entries.len(), 12);
    assert_eq!(snapshot.leaf_id_str(), Some("a1000012"));
    let branch: Vec<&str> = snapshot.branch().iter().map(|entry| entry["id"].as_str().unwrap()).collect();
    assert_eq!(branch, ["a1000001", "a1000002", "a1000003", "a1000004", "a1000011", "a1000012"]);
}

#[test]
fn migrates_v1_and_v2_sessions_in_memory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.jsonl");
    let content = [
        r#"{"type":"session","id":"s1","timestamp":"t","cwd":"/w"}"#,
        r#"{"type":"message","timestamp":"t","message":{"role":"user","content":"hi","timestamp":1}}"#,
        "not json",
        r#"{"type":"message","timestamp":"t","message":{"role":"hookMessage","customType":"x","content":"c"}}"#,
        r#"{"type":"compaction","timestamp":"t","summary":"s","firstKeptEntryIndex":1,"tokensBefore":5}"#,
        "",
    ]
    .join("\n");
    std::fs::write(&path, &content).unwrap();
    let snapshot = SessionSnapshot::load(&path).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "the file is left alone");
    assert_eq!(snapshot.header["version"], 3);
    let ids: Vec<&str> = snapshot.entries.iter().map(|entry| entry["id"].as_str().unwrap()).collect();
    assert!(ids.iter().all(|id| id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())));
    assert_eq!(snapshot.entries[0]["parentId"], Value::Null);
    assert_eq!(snapshot.entries[1]["parentId"], ids[0]);
    assert_eq!(snapshot.entries[1]["message"]["role"], "custom");
    let compaction = snapshot.entries[2].as_object().unwrap();
    assert_eq!(compaction["firstKeptEntryId"], ids[0]);
    assert!(!compaction.contains_key("firstKeptEntryIndex"));
    let keys: Vec<&String> = compaction.keys().collect();
    assert_eq!(keys, ["type", "timestamp", "summary", "tokensBefore", "id", "parentId", "firstKeptEntryId"]);
}

#[test]
fn headerless_and_empty_files_get_a_fresh_header() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, "").unwrap();
    let snapshot = SessionSnapshot::load(&empty).unwrap();
    assert!(snapshot.entries.is_empty());
    assert_eq!(snapshot.leaf_id, Some(Value::Null));
    let keys: Vec<&String> = snapshot.header.keys().collect();
    assert_eq!(keys, ["type", "version", "id", "timestamp", "cwd"]);

    let headerless = dir.path().join("headerless.jsonl");
    std::fs::write(&headerless, "{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null}\n").unwrap();
    let snapshot = SessionSnapshot::load(&headerless).unwrap();
    assert_eq!(snapshot.header["type"], "session");
    assert_eq!(snapshot.leaf_id_str(), Some("m1"));
}

#[test]
fn branch_jsonl_relinks_parents_and_appends_trailing_entries() {
    let snapshot = SessionSnapshot::load(&fixture()).unwrap();
    let tools = [ExportTool { name: "bash".into(), description: None, parameters: json!({"type": "object"}) }];
    let trailing =
        |parent: Option<&str>, timestamp: &str| create_share_trailing_entries("prompt", &tools, parent, timestamp);
    let text = serialize_at(&snapshot, Some(&trailing), "2026-02-03T04:05:06.007Z");
    let records = records(&text);
    assert_eq!(records[0]["cwd"], snapshot.cwd().display().to_string());
    #[cfg(unix)]
    assert_eq!(
        text.lines().next().unwrap(),
        r#"{"type":"session","version":3,"id":"0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b","timestamp":"2026-02-03T04:05:06.007Z","cwd":"/home/user/project"}"#
    );
    let ids: Vec<&str> = records[1..7].iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["a1000001", "a1000002", "a1000003", "a1000004", "a1000011", "a1000012"]);
    let parents: Vec<Value> = records[1..7].iter().map(|r| r["parentId"].clone()).collect();
    assert_eq!(
        parents,
        [Value::Null, "a1000001".into(), "a1000002".into(), "a1000003".into(), "a1000004".into(), "a1000011".into()]
    );
    // Relinking keeps `parentId` in its original position.
    assert!(text.lines().nth(5).unwrap().starts_with(r#"{"type":"message","id":"a1000011","parentId":"a1000004","#));

    let share = &records[7];
    assert_eq!(share["customType"], "pi.share");
    assert_eq!(share["parentId"], "a1000012");
    assert_eq!(share["timestamp"], "2026-02-03T04:05:06.007Z");
    assert_eq!(
        share["data"],
        json!({"systemPrompt": "prompt", "tools": [{"name": "bash", "parameters": {"type": "object"}}]})
    );
    assert_eq!(share["id"].as_str().unwrap().len(), 8);
}

#[test]
fn jsonl_export_resolves_paths_and_creates_directories() {
    let dir = tempfile::tempdir().unwrap();
    let path = export_session_to_jsonl(&fixture(), Some("nested/out.jsonl"), dir.path(), None).unwrap();
    assert_eq!(path, dir.path().join("nested/out.jsonl"));
    let lines = records(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(lines.len(), 7);
    assert!(lines.iter().all(|r| r["customType"] != "pi.share"));

    let path = export_session_to_jsonl(&fixture(), None, dir.path(), None).unwrap();
    let name = path.file_name().unwrap().to_str().unwrap();
    assert!(name.starts_with("session-") && name.ends_with("Z.jsonl") && !name.contains(':'), "{name}");

    let share = dir.path().join("share.jsonl");
    export_session_for_share(&fixture(), &share, "sys", &[]).unwrap();
    let lines = records(&std::fs::read_to_string(&share).unwrap());
    assert_eq!(lines.last().unwrap()["data"], json!({"systemPrompt": "sys", "tools": []}));
}

#[test]
fn iso_timestamps_match_to_iso_string() {
    assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso_timestamp(1_767_323_047_123), "2026-01-02T03:04:07.123Z");
    assert_eq!(iso_timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
}
