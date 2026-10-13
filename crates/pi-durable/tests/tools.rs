//! Upstream `test/tools.test.ts`: the coding tools run against a call api without a Harness (`FakeApi`), with the
//! environment hooks of `support/tools_support.rs`. The bash and powershell cases are in `tools/shell.rs`.

#[path = "tools/shell.rs"]
mod shell;
#[path = "support/tools_support.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pi_ai::ContentBlock;
use pi_chord::{AbortController, BACKGROUND_CONTEXT, with_abort_signal};
use pi_durable::env::{ExecutionEnv, LocalEnvOptions, LocalExecutionEnv, SharedEnv};
use pi_durable::harness::DiagnosticSeverity;
use pi_durable::tools::image::detect_supported_image_mime_type;
use pi_durable::tools::{ReadToolOptions, create_edit_tool, execute_edit, execute_read, execute_write};
use serde_json::{Value, json};
use support::*;
use tempfile::TempDir;
use tokio::sync::Notify;

fn temp_env() -> (TempDir, Arc<LocalExecutionEnv>) {
    let dir = tempfile::tempdir().unwrap();
    let env = LocalExecutionEnv::new(LocalEnvOptions::new(dir.path().to_str().unwrap()));
    (dir, Arc::new(env))
}

fn hooked(dir: &TempDir) -> HookedEnv {
    HookedEnv::new(dir.path().to_str().unwrap())
}

async fn write(env: &dyn ExecutionEnv, path: &str, content: &[u8]) {
    env.write_file(path, content, &ctx()).await.unwrap();
}

async fn read_text(env: &dyn ExecutionEnv, path: &str) -> String {
    env.read_text_file(path, &ctx()).await.unwrap()
}

async fn read(env: SharedEnv, value: Value) -> pi_durable::DurableResult<pi_durable::harness::ToolExecutionResult> {
    execute_read(&ReadToolOptions::default(), args(value), FakeApi::new(Some(env)), ctx()).await
}

async fn edit(env: SharedEnv, value: Value) -> pi_durable::DurableResult<pi_durable::harness::ToolExecutionResult> {
    execute_edit(args(value), FakeApi::new(Some(env)), ctx()).await
}

/// Blocks the write of `"first\n"` until `finish` is notified, and records whether `"second\n"` was written.
struct Blocking {
    started: Arc<Notify>,
    finish: Arc<Notify>,
    second: Arc<AtomicBool>,
}

fn blocking(env: &mut HookedEnv, first: &'static str, seconds: &'static [&'static str], detach: bool) -> Blocking {
    let gate = Blocking { started: Arc::default(), finish: Arc::default(), second: Arc::default() };
    let (started, finish, second) = (gate.started.clone(), gate.finish.clone(), gate.second.clone());
    env.on_write = Some(Arc::new(move |content| {
        let (started, finish, second) = (started.clone(), finish.clone(), second.clone());
        Box::pin(async move {
            if content == first.as_bytes() {
                started.notify_one();
                finish.notified().await;
                return detach;
            }
            if seconds.iter().any(|text| text.as_bytes() == content) {
                second.store(true, Ordering::SeqCst);
            }
            false
        })
    }));
    gate
}

#[tokio::test]
async fn fail_with_an_ordinary_error_when_no_environment_is_configured() {
    let options = ReadToolOptions::default();
    let result = execute_read(&options, args(json!({ "path": "x" })), FakeApi::new(None), ctx()).await;
    assert!(result.unwrap_err().to_string().contains("No execution environment"));
}

#[test]
fn detects_the_complete_gif_signatures() {
    for signature in ["GIF87a", "GIF89a"] {
        assert_eq!(detect_supported_image_mime_type(signature.as_bytes()), Some("image/gif"));
    }
}

#[tokio::test]
async fn reads_text_with_offsets_and_limits_and_reports_continuation_as_a_diagnostic() {
    let (_dir, env) = temp_env();
    let text = (1..=100).map(|index| format!("Line {index}")).collect::<Vec<_>>().join("\n");
    write(env.as_ref(), "test.txt", text.as_bytes()).await;
    let result = read(env, json!({ "path": "test.txt", "offset": 41, "limit": 20 })).await.unwrap();
    let output = text_output(&result);
    assert!(!output.contains("Line 40") && output.contains("Line 41") && output.contains("Line 60"));
    assert!(!output.contains("Line 61") && !output.contains("more lines"));
    assert_eq!(messages(&result), ["40 more lines in file. Use offset=61 to continue."]);
}

#[tokio::test]
async fn truncates_large_text_by_line_count() {
    let (_dir, env) = temp_env();
    let text = (1..=2500).map(|index| format!("Line {index}")).collect::<Vec<_>>().join("\n");
    write(env.as_ref(), "large.txt", text.as_bytes()).await;
    let result = read(env, json!({ "path": "large.txt" })).await.unwrap();
    assert_eq!(messages(&result), ["Showing lines 1-2000 of 2500. Use offset=2001 to continue."]);
    assert_eq!(result.diagnostics.as_ref().unwrap()[0].code.as_deref(), Some("truncated"));
    let truncation = Value::from(result.details.unwrap())["truncation"].clone();
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["truncatedBy"], json!("lines"));
    assert_eq!((truncation["totalLines"].clone(), truncation["outputLines"].clone()), (json!(2500), json!(2000)));
}

#[tokio::test]
async fn does_not_count_a_trailing_newline_as_an_extra_line_at_the_truncation_limit() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "exact.txt", format!("{}\n", vec!["x"; 2000].join("\n")).as_bytes()).await;
    let result = read(env, json!({ "path": "exact.txt" })).await.unwrap();
    assert_eq!(result.details, None);
    assert_eq!(result.diagnostics, Some(Vec::new()));
}

#[tokio::test]
async fn shows_the_start_of_a_line_longer_than_the_byte_limit() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "long.txt", format!("{}\nnext\n", "é".repeat(40_000)).as_bytes()).await;
    let result = read(env, json!({ "path": "long.txt" })).await.unwrap();
    // Two-byte characters: the cut lands on a character boundary at or below the limit.
    assert_eq!(text_output(&result), "é".repeat(25_600));
    assert_eq!(
        messages(&result),
        [
            "Line 1 is 78.1KB, exceeds the 50.0KB limit; showing its first 50.0KB. Use bash: sed -n '1p' long.txt | tail -c +51201"
        ]
    );
    let truncation = Value::from(result.details.unwrap())["truncation"].clone();
    assert_eq!(truncation["truncated"], json!(true));
    assert_eq!(truncation["firstLineExceedsLimit"], json!(true));
    assert_eq!((truncation["outputBytes"].clone(), truncation["outputLines"].clone()), (json!(51_200), json!(1)));
    assert!(truncation.get("content").is_none());
}

#[tokio::test]
async fn rejects_offsets_beyond_the_file() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "short.txt", b"one\ntwo\nthree").await;
    let error = read(env, json!({ "path": "short.txt", "offset": 100 })).await.unwrap_err();
    assert_eq!(error.to_string(), "Offset 100 is beyond end of file (3 lines total)");
}

#[tokio::test]
async fn reads_a_log_that_grows_while_it_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = hooked(&dir);
    write(&env, "app.log", b"one\ntwo\n").await;
    let log = dir.path().join("app.log");
    // Every read appends, as a busy writer would; the scanned lines stay as they were.
    env.on_reader = Some(Arc::new(move |inner| {
        let log = log.clone();
        let on_read = Arc::new(move |_: u64, _: u64| {
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"more\n").unwrap();
        });
        Box::new(ObservedReader { inner, on_read })
    }));
    let result = read(Arc::new(env), json!({ "path": "app.log", "limit": 2 })).await.unwrap();
    assert_eq!(text_output(&result), "one\ntwo");
}

#[tokio::test]
async fn returns_images_by_content_as_one_image_block() {
    let (_dir, env) = temp_env();
    let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGD4DwABBAEAX+XDSwAAAABJRU5ErkJggg==";
    use base64::Engine;
    write(env.as_ref(), "image.txt", &base64::engine::general_purpose::STANDARD.decode(data).unwrap()).await;
    let result = read(env, json!({ "path": "image.txt" })).await.unwrap();
    let image = ContentBlock::Image { data: data.into(), mime_type: "image/png".into() };
    assert_eq!(result.output, Some(vec![image]));
    assert_eq!(result.is_error, None);
    assert_eq!(messages(&result), ["Read image file [image/png]."]);
    assert_eq!(result.diagnostics.unwrap()[0].severity, DiagnosticSeverity::Info);
}

#[tokio::test]
async fn writes_files_and_creates_parent_directories() {
    let (_dir, env) = temp_env();
    let call = args(json!({ "path": "nested/dir/file.txt", "content": "hello" }));
    let result = execute_write(call, FakeApi::new(Some(env.clone())), ctx()).await.unwrap();
    assert_eq!(text_output(&result), "Successfully wrote to nested/dir/file.txt");
    assert_eq!(read_text(env.as_ref(), "nested/dir/file.txt").await, "hello");
}

#[tokio::test]
async fn keeps_the_mutation_queue_locked_until_an_aborted_write_settles() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = hooked(&dir);
    let gate = blocking(&mut env, "first\n", &["second\n"], false);
    let env: SharedEnv = Arc::new(env);
    let controller = AbortController::new();
    let aborting = with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT);
    let call = args(json!({ "path": "file.txt", "content": "first\n" }));
    let first = tokio::spawn(execute_write(call, FakeApi::new(Some(env.clone())), aborting));
    gate.started.notified().await;
    controller.abort(None);
    let call = args(json!({ "path": "file.txt", "content": "second\n" }));
    let second = tokio::spawn(execute_write(call, FakeApi::new(Some(env.clone())), ctx()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!gate.second.load(Ordering::SeqCst));
    gate.finish.notify_one();
    assert!(first.await.unwrap().is_err());
    second.await.unwrap().unwrap();
    assert_eq!(read_text(env.as_ref(), "file.txt").await, "second\n");
}

#[tokio::test]
async fn applies_disjoint_edits_and_returns_both_diff_formats() {
    let (_dir, env) = temp_env();
    let original = "alpha\nbeta\ngamma\ndelta\n";
    write(env.as_ref(), "edit.txt", original.as_bytes()).await;
    let edits = json!([{ "oldText": "alpha\n", "newText": "ALPHA\n" }, { "oldText": "gamma\n", "newText": "GAMMA\n" }]);
    let result = edit(env.clone(), json!({ "path": "edit.txt", "edits": edits })).await.unwrap();
    assert_eq!(text_output(&result), "Successfully replaced 2 block(s) in edit.txt.");
    let details = Value::from(result.details.unwrap());
    let diff = details["diff"].as_str().unwrap();
    assert!(diff.contains("ALPHA") && diff.contains("GAMMA"));
    // Upstream applies the patch with jsdiff; no Rust patch applier is a dependency, so its hunks are checked.
    let patch = details["patch"].as_str().unwrap();
    assert!(patch.starts_with("--- edit.txt\n+++ edit.txt\n@@ -1,4 +1,4 @@\n"));
    assert!(patch.contains("-alpha\n+ALPHA\n beta\n-gamma\n+GAMMA\n delta\n"));
    assert_eq!(read_text(env.as_ref(), "edit.txt").await, "ALPHA\nbeta\nGAMMA\ndelta\n");
}

#[test]
fn repairs_edits_sent_as_a_json_string_a_single_object_or_top_level_old_text_new_text_without_mutating_them() {
    let prepare = create_edit_tool().prepare_arguments.unwrap();
    let edit = json!({ "oldText": "a", "newText": "b" });
    let as_string = json!({ "path": "f", "edits": json!([edit]).to_string() });
    assert_eq!(prepare(&as_string).unwrap(), json!({ "path": "f", "edits": [edit] }));
    assert_eq!(as_string["edits"], json!(json!([edit]).to_string()));
    let single = json!({ "path": "f", "edits": edit.to_string() });
    assert_eq!(prepare(&single).unwrap(), json!({ "path": "f", "edits": [edit] }));
    assert_eq!(prepare(&json!({ "path": "f", "edits": edit })).unwrap(), json!({ "path": "f", "edits": [edit] }));
    let legacy = json!({ "path": "f", "edits": [edit], "oldText": "c", "newText": "d" });
    let repaired = json!({ "path": "f", "edits": [edit, { "oldText": "c", "newText": "d" }] });
    assert_eq!(prepare(&legacy).unwrap(), repaired);
    let broken = json!({ "path": "f", "edits": "not json" });
    assert_eq!(prepare(&broken).unwrap(), broken);
}

#[tokio::test]
async fn matches_all_edits_against_the_original_and_rejects_overlaps() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "edit.txt", b"one\ntwo\nthree\n").await;
    let edits = json!([
        { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
        { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" }
    ]);
    let error = edit(env.clone(), json!({ "path": "edit.txt", "edits": edits })).await.unwrap_err();
    assert!(error.to_string().contains("overlap"));
    assert_eq!(read_text(env.as_ref(), "edit.txt").await, "one\ntwo\nthree\n");
}

#[tokio::test]
async fn rejects_missing_and_duplicate_target_text() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "edit.txt", b"foo foo foo").await;
    let missing = json!({ "path": "edit.txt", "edits": [{ "oldText": "bar", "newText": "baz" }] });
    assert!(edit(env.clone(), missing).await.unwrap_err().to_string().contains("Could not find the exact text"));
    let duplicate = json!({ "path": "edit.txt", "edits": [{ "oldText": "foo", "newText": "bar" }] });
    assert!(edit(env, duplicate).await.unwrap_err().to_string().contains("Found 3 occurrences"));
}

#[tokio::test]
async fn keeps_the_mutation_queue_locked_until_an_aborted_edit_write_settles() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = hooked(&dir);
    write(&env, "file.txt", b"alpha\nbeta\n").await;
    // The first edit's write ignores the cancellation, as upstream's write with BACKGROUND_CONTEXT does.
    let gate = blocking(&mut env, "ALPHA\nbeta\n", &["ALPHA\nBETA\n", "alpha\nBETA\n"], true);
    let env: SharedEnv = Arc::new(env);
    let controller = AbortController::new();
    let aborting = with_abort_signal(&controller.signal(), &BACKGROUND_CONTEXT);
    let call = args(json!({ "path": "file.txt", "edits": [{ "oldText": "alpha", "newText": "ALPHA" }] }));
    let first = tokio::spawn(execute_edit(call, FakeApi::new(Some(env.clone())), aborting));
    gate.started.notified().await;
    controller.abort(None);
    let call = args(json!({ "path": "file.txt", "edits": [{ "oldText": "beta", "newText": "BETA" }] }));
    let second = tokio::spawn(execute_edit(call, FakeApi::new(Some(env.clone())), ctx()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!gate.second.load(Ordering::SeqCst));
    gate.finish.notify_one();
    assert_eq!(first.await.unwrap().unwrap_err().to_string(), "Operation aborted");
    second.await.unwrap().unwrap();
    assert_eq!(read_text(env.as_ref(), "file.txt").await, "ALPHA\nBETA\n");
}

#[cfg(unix)]
#[tokio::test]
async fn serializes_concurrent_edits_through_canonical_and_symlink_paths() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = hooked(&dir);
    env.read_delay = Some(Duration::from_millis(20));
    write(&env, "target.txt", b"alpha\nbeta\ngamma\n").await;
    std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
    let env: SharedEnv = Arc::new(env);
    let first = json!({ "path": "target.txt", "edits": [{ "oldText": "alpha", "newText": "ALPHA" }] });
    let second = json!({ "path": "link.txt", "edits": [{ "oldText": "beta", "newText": "BETA" }] });
    let (a, b) = tokio::join!(edit(env.clone(), first), edit(env.clone(), second));
    a.unwrap();
    b.unwrap();
    assert_eq!(read_text(env.as_ref(), "target.txt").await, "ALPHA\nBETA\ngamma\n");
}

#[tokio::test]
async fn serializes_edits_of_one_file_across_environment_objects_of_one_file_system() {
    let dir = tempfile::tempdir().unwrap();
    let (mut first, mut second) = (hooked(&dir), hooked(&dir));
    first.read_delay = Some(Duration::from_millis(20));
    second.read_delay = Some(Duration::from_millis(20));
    write(&first, "file.txt", b"alpha\nbeta\n").await;
    let (first, second): (SharedEnv, SharedEnv) = (Arc::new(first), Arc::new(second));
    let a = json!({ "path": "file.txt", "edits": [{ "oldText": "alpha", "newText": "ALPHA" }] });
    let b = json!({ "path": "file.txt", "edits": [{ "oldText": "beta", "newText": "BETA" }] });
    let (a, b) = tokio::join!(edit(first.clone(), a), edit(second, b));
    a.unwrap();
    b.unwrap();
    assert_eq!(read_text(first.as_ref(), "file.txt").await, "ALPHA\nBETA\n");
}

#[cfg(unix)]
#[tokio::test]
async fn serializes_a_new_file_created_through_a_symlinked_directory_with_its_canonical_path() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = hooked(&dir);
    let gate = blocking(&mut env, "first\n", &["second\n"], false);
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    let env: SharedEnv = Arc::new(env);
    let call = args(json!({ "path": "link/new.txt", "content": "first\n" }));
    let first = tokio::spawn(execute_write(call, FakeApi::new(Some(env.clone())), ctx()));
    gate.started.notified().await;
    let call = args(json!({ "path": "real/new.txt", "content": "second\n" }));
    let second = tokio::spawn(execute_write(call, FakeApi::new(Some(env.clone())), ctx()));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!gate.second.load(Ordering::SeqCst));
    gate.finish.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(read_text(env.as_ref(), "real/new.txt").await, "second\n");
}

#[cfg(unix)]
#[tokio::test]
async fn keys_a_missing_file_whose_name_contains_a_backslash_like_the_created_file() {
    use pi_durable::env::FileSystem;
    use pi_durable::tools::file_mutation_queue;
    let (_dir, env) = temp_env();
    let (created, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let entered = Arc::new(AtomicBool::new(false));
    let first = {
        let (env, created, release) = (env.clone(), created.clone(), release.clone());
        tokio::spawn(async move {
            let run = || async {
                env.write_file("a\\b.txt", b"first\n", &ctx()).await.unwrap();
                created.notify_one();
                release.notified().await;
                Ok(())
            };
            file_mutation_queue::with_file_mutation_queue(env.as_ref(), "a\\b.txt", run, &ctx()).await
        })
    };
    created.notified().await;
    let second = {
        let (env, entered) = (env.clone(), entered.clone());
        tokio::spawn(async move {
            let run = || async {
                entered.store(true, Ordering::SeqCst);
                Ok(())
            };
            file_mutation_queue::with_file_mutation_queue(env.as_ref(), "a\\b.txt", run, &ctx()).await
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!entered.load(Ordering::SeqCst));
    release.notify_one();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert!(entered.load(Ordering::SeqCst));
}

#[tokio::test]
async fn does_not_serialize_the_same_path_on_different_file_systems() {
    let dir = tempfile::tempdir().unwrap();
    let mut local = hooked(&dir);
    let gate = blocking(&mut local, "first\n", &["second\n"], false);
    let mut other = hooked(&dir);
    other.id = Some("other".into());
    let other_gate = blocking(&mut other, "unused", &["second\n"], false);
    let local: SharedEnv = Arc::new(local);
    let call = args(json!({ "path": "file.txt", "content": "first\n" }));
    let blocked = tokio::spawn(execute_write(call, FakeApi::new(Some(local)), ctx()));
    gate.started.notified().await;
    let call = args(json!({ "path": "file.txt", "content": "second\n" }));
    execute_write(call, FakeApi::new(Some(Arc::new(other))), ctx()).await.unwrap();
    assert!(other_gate.second.load(Ordering::SeqCst));
    gate.finish.notify_one();
    blocked.await.unwrap().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn edits_regular_files_through_symlinks() {
    let (dir, env) = temp_env();
    write(env.as_ref(), "target.txt", b"before\n").await;
    std::os::unix::fs::symlink("target.txt", dir.path().join("link.txt")).unwrap();
    edit(env.clone(), json!({ "path": "link.txt", "edits": [{ "oldText": "before", "newText": "after" }] }))
        .await
        .unwrap();
    assert_eq!(read_text(env.as_ref(), "target.txt").await, "after\n");
}

#[tokio::test]
async fn preserves_bom_and_crlf_line_endings() {
    let (_dir, env) = temp_env();
    write(env.as_ref(), "edit.txt", "\u{FEFF}one\r\ntwo\r\n".as_bytes()).await;
    edit(env.clone(), json!({ "path": "edit.txt", "edits": [{ "oldText": "two", "newText": "TWO" }] })).await.unwrap();
    assert_eq!(read_text(env.as_ref(), "edit.txt").await, "\u{FEFF}one\r\nTWO\r\n");
}
