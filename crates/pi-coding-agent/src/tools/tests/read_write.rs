use pi_ai::ContentBlock;
use serde_json::json;

use super::{run, text};
use crate::tools::truncate::DEFAULT_MAX_LINES;

fn numbered(n: usize) -> String {
    (1..=n).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn read_whole_file_relative_to_cwd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
    let result = run("read", dir.path(), json!({ "path": "a.txt" })).await.unwrap();
    assert_eq!(text(&result), "hello\nworld");
    assert!(result.details.is_none());
}

#[tokio::test]
async fn read_offset_and_limit_with_continuation_hint() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), numbered(10)).unwrap();
    let result = run("read", dir.path(), json!({ "path": "a.txt", "offset": 3, "limit": 2 })).await.unwrap();
    assert_eq!(text(&result), "line 3\nline 4\n\n[6 more lines in file. Use offset=5 to continue.]");
    let result = run("read", dir.path(), json!({ "path": "a.txt", "offset": 9 })).await.unwrap();
    assert_eq!(text(&result), "line 9\nline 10");
}

#[tokio::test]
async fn read_offset_beyond_end_is_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo").unwrap();
    let err = run("read", dir.path(), json!({ "path": "a.txt", "offset": 5 })).await.unwrap_err();
    assert_eq!(err.to_string(), "Offset 5 is beyond end of file (2 lines total)");
}

#[tokio::test]
async fn read_missing_file_is_error() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run("read", dir.path(), json!({ "path": "nope.txt" })).await.is_err());
}

#[tokio::test]
async fn read_truncates_by_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big.txt"), numbered(DEFAULT_MAX_LINES + 500)).unwrap();
    let result = run("read", dir.path(), json!({ "path": "big.txt" })).await.unwrap();
    let out = text(&result);
    assert!(out.ends_with("[Showing lines 1-2000 of 2500. Use offset=2001 to continue.]"), "{out}");
    let details = result.details.unwrap();
    assert_eq!(details["truncation"]["truncatedBy"], "lines");
    assert_eq!(details["truncation"]["outputLines"], 2000);
}

#[tokio::test]
async fn read_truncates_by_bytes_and_long_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let line = "x".repeat(100);
    std::fs::write(dir.path().join("wide.txt"), vec![line.as_str(); 1000].join("\n")).unwrap();
    let out = text(&run("read", dir.path(), json!({ "path": "wide.txt" })).await.unwrap());
    assert!(out.ends_with("of 1000 (50.0KB limit). Use offset=507 to continue.]"), "{}", &out[out.len() - 120..]);

    std::fs::write(dir.path().join("huge.txt"), "y".repeat(60 * 1024)).unwrap();
    let out = text(&run("read", dir.path(), json!({ "path": "huge.txt" })).await.unwrap());
    assert_eq!(out, "[Line 1 is 60.0KB, exceeds 50.0KB limit. Use bash: sed -n '1p' huge.txt | head -c 51200]");
}

fn tiny_png() -> Vec<u8> {
    let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
    png.extend_from_slice(b"IHDR");
    png.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1f, 0x15, 0xc4, 0x89]);
    png.extend_from_slice(&[0, 0, 0, 0]);
    png.extend_from_slice(b"IEND");
    png.extend_from_slice(&[0xae, 0x42, 0x60, 0x82]);
    png
}

#[tokio::test]
async fn read_image_returns_image_block() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("shot.dat"), tiny_png()).unwrap();
    let result = run("read", dir.path(), json!({ "path": "shot.dat" })).await.unwrap();
    assert_eq!(text(&result), "Read image file [image/png]");
    let ContentBlock::Image { data, mime_type } = &result.content[1] else { panic!("expected image") };
    assert_eq!(mime_type, "image/png");
    use base64::Engine;
    assert_eq!(base64::engine::general_purpose::STANDARD.decode(data).unwrap(), tiny_png());
}

#[test]
fn detects_image_magic_bytes() {
    use crate::tools::read::detect_supported_image_mime_type as detect;
    assert_eq!(detect(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
    assert_eq!(detect(b"GIF89a...."), Some("image/gif"));
    assert_eq!(detect(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(detect(&tiny_png()), Some("image/png"));
    assert_eq!(detect(b"plain text"), None);
}

#[tokio::test]
async fn write_creates_parent_dirs_and_reports_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let result = run("write", dir.path(), json!({ "path": "a/b/c.txt", "content": "héllo" })).await.unwrap();
    assert_eq!(text(&result), "Successfully wrote 6 bytes to a/b/c.txt");
    assert_eq!(std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "héllo");
    run("write", dir.path(), json!({ "path": "a/b/c.txt", "content": "new" })).await.unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(), "new");
}

#[tokio::test]
async fn mutation_queue_serializes_same_file() {
    use crate::tools::mutation_queue::with_file_mutation_queue;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    let active = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let tasks: Vec<_> = (0..5)
        .map(|_| {
            let (path, active, max_seen) = (path.clone(), active.clone(), max_seen.clone());
            tokio::spawn(async move {
                with_file_mutation_queue(&path, || async {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(max_seen.load(Ordering::SeqCst), 1);
}
