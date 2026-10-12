use serde_json::json;

use super::{run, text};

fn setup(content: &[u8]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.txt"), content).unwrap();
    dir
}

fn read(dir: &tempfile::TempDir) -> String {
    std::fs::read_to_string(dir.path().join("f.txt")).unwrap()
}

#[tokio::test]
async fn unique_replacement_with_diff_details() {
    let dir = setup(b"alpha\nbeta\ngamma\n");
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "beta", "newText": "BETA" }] });
    let result = run("edit", dir.path(), args).await.unwrap();
    assert_eq!(text(&result), "Successfully replaced 1 block(s) in f.txt.");
    assert_eq!(read(&dir), "alpha\nBETA\ngamma\n");
    let details = result.details.unwrap();
    assert_eq!(details["diff"], " 1 alpha\n-2 beta\n+2 BETA\n 3 gamma");
    assert_eq!(details["firstChangedLine"], 2);
    let patch = details["patch"].as_str().unwrap();
    assert!(patch.starts_with("--- f.txt\n+++ f.txt\n@@ -1,3 +1,3 @@\n"), "{patch}");
    assert!(patch.contains("-beta\n+BETA\n"));
}

#[tokio::test]
async fn ambiguous_match_is_error() {
    let dir = setup(b"x = 1\nx = 1\n");
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "x = 1", "newText": "y" }] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert!(err.starts_with("Found 2 occurrences of the text in f.txt. The text must be unique."), "{err}");
    assert_eq!(read(&dir), "x = 1\nx = 1\n");
}

#[tokio::test]
async fn missing_match_and_missing_file_are_errors() {
    let dir = setup(b"hello\n");
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "absent", "newText": "y" }] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert!(err.starts_with("Could not find the exact text in f.txt."), "{err}");
    let args = json!({ "path": "nope.txt", "edits": [{ "oldText": "a", "newText": "b" }] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert_eq!(err, "Could not edit file: nope.txt. Error code: ENOENT.");
    let err = run("edit", dir.path(), json!({ "path": "f.txt", "edits": [] })).await.unwrap_err().to_string();
    assert!(err.contains("edits must contain at least one replacement"));
}

#[tokio::test]
async fn multiple_edits_against_original() {
    let dir = setup(b"one\ntwo\nthree\nfour\n");
    let args = json!({ "path": "f.txt", "edits": [
        { "oldText": "four", "newText": "4" },
        { "oldText": "one", "newText": "1" }
    ] });
    let result = run("edit", dir.path(), args).await.unwrap();
    assert_eq!(text(&result), "Successfully replaced 2 block(s) in f.txt.");
    assert_eq!(read(&dir), "1\ntwo\nthree\n4\n");

    let args = json!({ "path": "f.txt", "edits": [
        { "oldText": "1\ntwo", "newText": "a" },
        { "oldText": "two\nthree", "newText": "b" }
    ] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert!(err.contains("overlap"), "{err}");
    let args =
        json!({ "path": "f.txt", "edits": [{ "oldText": "1", "newText": "x" }, { "oldText": "zzz", "newText": "y" }] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert!(err.starts_with("Could not find edits[1] in f.txt."), "{err}");
}

#[tokio::test]
async fn preserves_crlf_and_bom() {
    let dir = setup("\u{FEFF}first\r\nsecond\r\nthird\r\n".as_bytes());
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "first\nsecond", "newText": "1st\n2nd" }] });
    run("edit", dir.path(), args).await.unwrap();
    assert_eq!(read(&dir), "\u{FEFF}1st\r\n2nd\r\nthird\r\n");
}

#[tokio::test]
async fn fuzzy_match_keeps_untouched_lines() {
    let dir = setup("keep  \nsay \u{201C}hi\u{201D}\nend  \n".as_bytes());
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "say \"hi\"", "newText": "say \"bye\"" }] });
    run("edit", dir.path(), args).await.unwrap();
    assert_eq!(read(&dir), "keep  \nsay \"bye\"\nend  \n");
}

#[tokio::test]
async fn accepts_legacy_and_stringified_edit_arguments() {
    let dir = setup(b"a b c\n");
    run("edit", dir.path(), json!({ "path": "f.txt", "oldText": "a", "newText": "A" })).await.unwrap();
    let edits = r#"[{"oldText":"b","newText":"B"}]"#;
    run("edit", dir.path(), json!({ "path": "f.txt", "edits": edits })).await.unwrap();
    run("edit", dir.path(), json!({ "path": "f.txt", "edits": { "oldText": "c", "newText": "C" } })).await.unwrap();
    assert_eq!(read(&dir), "A B C\n");
}

#[tokio::test]
async fn identical_replacement_is_error() {
    let dir = setup(b"same\n");
    let args = json!({ "path": "f.txt", "edits": [{ "oldText": "same", "newText": "same" }] });
    let err = run("edit", dir.path(), args).await.unwrap_err().to_string();
    assert!(err.starts_with("No changes made to f.txt."), "{err}");
}

#[test]
fn display_diff_elides_distant_context() {
    use crate::tools::edit_diff::generate_diff_string;
    let old: String = (1..=20).map(|i| format!("l{i}\n")).collect();
    let new = old.replace("l10\n", "X\n");
    let (diff, first) = generate_diff_string(&old, &new);
    assert_eq!(first, Some(10));
    let lines: Vec<&str> = diff.lines().collect();
    assert_eq!(lines.first(), Some(&"    ..."));
    assert!(lines.contains(&"-10 l10") && lines.contains(&"+10 X"));
    assert_eq!(lines.last(), Some(&"    ..."));
    assert_eq!(lines.len(), 1 + 4 + 2 + 4 + 1);
}
