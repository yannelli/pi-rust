use std::path::Path;

use serde_json::json;

use super::{run, text};

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A git-like tree: `.git` marker, a .gitignore, hidden files, and nested sources.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    write(root, ".gitignore", "ignored/\n*.log\n");
    write(root, "src/main.rs", "fn main() {\n    println!(\"Hello\");\n}\n");
    write(root, "src/lib.rs", "pub fn hello() {}\n// TODO: more\n");
    write(root, "src/util/helpers.rs", "fn helper() {}\n// todo lower\n");
    write(root, "ignored/skip.rs", "fn main() {}\n");
    write(root, "debug.log", "fn main() {}\n");
    write(root, ".hidden.rs", "fn main() {}\n");
    write(root, "notes.txt", "a.b\naxb\n");
    dir
}

#[tokio::test]
async fn grep_regex_respects_gitignore_and_includes_hidden() {
    let dir = fixture();
    let out = text(&run("grep", dir.path(), json!({ "pattern": "fn ma.n" })).await.unwrap());
    assert_eq!(out, ".hidden.rs:1: fn main() {}\nsrc/main.rs:1: fn main() {");
}

#[tokio::test]
async fn grep_literal_and_ignore_case() {
    let dir = fixture();
    let out = text(&run("grep", dir.path(), json!({ "pattern": "a.b", "literal": true })).await.unwrap());
    assert_eq!(out, "notes.txt:1: a.b");
    let out = text(&run("grep", dir.path(), json!({ "pattern": "a.b" })).await.unwrap());
    assert_eq!(out, "notes.txt:1: a.b\nnotes.txt:2: axb");
    let out = text(&run("grep", dir.path(), json!({ "pattern": "todo", "ignoreCase": true })).await.unwrap());
    assert_eq!(out, "src/lib.rs:2: // TODO: more\nsrc/util/helpers.rs:2: // todo lower");
    let out = text(&run("grep", dir.path(), json!({ "pattern": "nothing-here" })).await.unwrap());
    assert_eq!(out, "No matches found");
}

#[tokio::test]
async fn grep_glob_context_limit_and_single_file() {
    let dir = fixture();
    let args = json!({ "pattern": "fn", "glob": "**/util/*.rs" });
    assert_eq!(text(&run("grep", dir.path(), args).await.unwrap()), "src/util/helpers.rs:1: fn helper() {}");
    let args = json!({ "pattern": "println", "path": "src", "context": 1 });
    let out = text(&run("grep", dir.path(), args).await.unwrap());
    assert_eq!(out, "main.rs-1- fn main() {\nmain.rs:2:     println!(\"Hello\");\nmain.rs-3- }");
    let result = run("grep", dir.path(), json!({ "pattern": "fn", "limit": 2 })).await.unwrap();
    let out = text(&result);
    assert!(out.ends_with("[2 matches limit reached. Use limit=4 for more, or refine pattern]"), "{out}");
    assert_eq!(result.details.unwrap()["matchLimitReached"], 2);
    let out = text(&run("grep", dir.path(), json!({ "pattern": "hello", "path": "src/lib.rs" })).await.unwrap());
    assert_eq!(out, "lib.rs:1: pub fn hello() {}");
    assert!(run("grep", dir.path(), json!({ "pattern": "x", "path": "missing" })).await.is_err());
    assert!(run("grep", dir.path(), json!({ "pattern": "(" })).await.is_err());
}

// Upstream #11398: a lone CR shifted every later context line by one.
#[tokio::test]
async fn grep_context_lines_stay_aligned_when_a_file_has_a_lone_cr() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "lone-cr.txt", "before\rprogress\nneedle\nafter");
    let args = json!({ "pattern": "needle", "path": "lone-cr.txt", "context": 1 });
    let out = text(&run("grep", dir.path(), args).await.unwrap());
    assert_eq!(out, "lone-cr.txt-1- beforeprogress\nlone-cr.txt:2: needle\nlone-cr.txt-3- after");
}

#[tokio::test]
async fn grep_truncates_long_lines() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "long.txt", &format!("match {}\n", "z".repeat(600)));
    let result = run("grep", dir.path(), json!({ "pattern": "match" })).await.unwrap();
    assert!(text(&result).contains("... [truncated]"));
    assert_eq!(result.details.unwrap()["linesTruncated"], true);
}

#[tokio::test]
async fn find_glob_respects_gitignore() {
    let dir = fixture();
    let out = text(&run("find", dir.path(), json!({ "pattern": "*.rs" })).await.unwrap());
    assert_eq!(out, ".hidden.rs\nsrc/lib.rs\nsrc/main.rs\nsrc/util/helpers.rs");
    let out = text(&run("find", dir.path(), json!({ "pattern": "src/*.rs" })).await.unwrap());
    assert_eq!(out, "src/lib.rs\nsrc/main.rs");
    let out = text(&run("find", dir.path(), json!({ "pattern": "util" })).await.unwrap());
    assert_eq!(out, "src/util/");
    let out = text(&run("find", dir.path(), json!({ "pattern": "*.log" })).await.unwrap());
    assert_eq!(out, "No files found matching pattern");
}

#[tokio::test]
async fn find_limit_and_missing_path() {
    let dir = fixture();
    let result = run("find", dir.path(), json!({ "pattern": "*.rs", "limit": 2 })).await.unwrap();
    let out = text(&result);
    assert!(out.starts_with(".hidden.rs\nsrc/lib.rs\n\n[2 results limit reached. Use limit=4"), "{out}");
    assert_eq!(result.details.unwrap()["resultLimitReached"], 2);
    let err = run("find", dir.path(), json!({ "pattern": "*", "path": "missing" })).await.unwrap_err();
    assert!(err.to_string().starts_with("Path not found: "));
}

#[tokio::test]
async fn ls_sorts_marks_dirs_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "b.txt", "");
    write(root, "A.txt", "");
    write(root, ".env", "");
    std::fs::create_dir(root.join("zdir")).unwrap();
    std::fs::create_dir(root.join("Cdir")).unwrap();
    let out = text(&run("ls", root, json!({})).await.unwrap());
    assert_eq!(out, ".env\nA.txt\nb.txt\nCdir/\nzdir/");
    let result = run("ls", root, json!({ "limit": 2 })).await.unwrap();
    assert_eq!(text(&result), ".env\nA.txt\n\n[2 entries limit reached. Use limit=4 for more]");
    assert_eq!(result.details.unwrap()["entryLimitReached"], 2);
    let out = text(&run("ls", root, json!({ "path": "zdir" })).await.unwrap());
    assert_eq!(out, "(empty directory)");
    let err = run("ls", root, json!({ "path": "b.txt" })).await.unwrap_err();
    assert!(err.to_string().starts_with("Not a directory: "));
    assert!(run("ls", root, json!({ "path": "nope" })).await.is_err());
}
