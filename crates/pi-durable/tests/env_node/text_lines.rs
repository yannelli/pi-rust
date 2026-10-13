//! `NodeExecutionEnv text line reader` cases.

use pi_durable::env::{FileErrorCode, FileSystem, TextLine};

use crate::support::{aborted_context, context, join, local_env, temp_dir};

fn line(text: &str, terminated: bool) -> TextLine {
    TextLine { text: text.to_string(), terminated }
}

#[tokio::test]
async fn reports_whether_each_line_was_newline_terminated_and_preserves_carriage_returns() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("lines.txt", b"one\r\n\ntwo\npartial", &ctx).await.unwrap();
    let reader = env.open_text_line_reader("lines.txt", &ctx).await.unwrap();
    let mut lines = Vec::new();
    while let Some(line) = reader.read_line(&ctx).await.unwrap() {
        lines.push(line);
    }
    assert_eq!(lines, [line("one\r", true), line("", true), line("two", true), line("partial", false)]);
    assert_eq!(reader.read_line(&ctx).await.unwrap(), None);
    reader.close(&ctx).await;
}

#[tokio::test]
async fn returns_no_lines_for_an_empty_file_and_one_terminated_empty_line_for_a_lone_newline() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("empty.txt", b"", &ctx).await.unwrap();
    env.write_file("newline.txt", b"\n", &ctx).await.unwrap();
    let empty = env.open_text_line_reader("empty.txt", &ctx).await.unwrap();
    assert_eq!(empty.read_line(&ctx).await.unwrap(), None);
    empty.close(&ctx).await;
    let newline = env.open_text_line_reader("newline.txt", &ctx).await.unwrap();
    assert_eq!(newline.read_line(&ctx).await.unwrap(), Some(line("", true)));
    assert_eq!(newline.read_line(&ctx).await.unwrap(), None);
    newline.close(&ctx).await;
}

#[tokio::test]
async fn decodes_multi_byte_characters_split_across_read_chunks_and_lines_longer_than_one_chunk() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    // The reader uses 64 KiB chunks; place a four-byte character across the first boundary.
    let first = format!("{}😀tail", "a".repeat(64 * 1024 - 2));
    let second = "é".repeat(100_000);
    env.write_file("large.txt", format!("{first}\n{second}").as_bytes(), &ctx).await.unwrap();
    let reader = env.open_text_line_reader("large.txt", &ctx).await.unwrap();
    assert_eq!(reader.read_line(&ctx).await.unwrap(), Some(line(&first, true)));
    assert_eq!(reader.read_line(&ctx).await.unwrap(), Some(line(&second, false)));
    assert_eq!(reader.read_line(&ctx).await.unwrap(), None);
    reader.close(&ctx).await;
}

/// Upstream gates a mocked `FileHandle.read` to abort while the read is pending; Rust has no such seam. An aborted
/// call must still consume no bytes, which this checks with an abort before the read.
#[tokio::test]
async fn rejects_a_cancelled_read_without_consuming_its_bytes() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("lines.txt", b"one\ntwo\n", &ctx).await.unwrap();
    let reader = env.open_text_line_reader("lines.txt", &ctx).await.unwrap();
    assert_eq!(reader.read_line(&aborted_context()).await.unwrap_err().code, FileErrorCode::Aborted);
    assert_eq!(reader.read_line(&ctx).await.unwrap(), Some(line("one", true)));
    assert_eq!(reader.read_line(&ctx).await.unwrap(), Some(line("two", true)));
    assert_eq!(reader.read_line(&ctx).await.unwrap(), None);
    reader.close(&ctx).await;
}

#[tokio::test]
async fn rejects_reads_after_close_and_closes_idempotently() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("lines.txt", b"one\n", &ctx).await.unwrap();
    let reader = env.open_text_line_reader("lines.txt", &ctx).await.unwrap();
    reader.close(&ctx).await;
    reader.close(&ctx).await;
    let error = reader.read_line(&ctx).await.unwrap_err();
    assert_eq!((error.code, error.path), (FileErrorCode::Invalid, Some(join(dir.path(), "lines.txt"))));
}

#[tokio::test]
async fn reports_missing_files_when_opening_a_reader() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let error = env.open_text_line_reader("missing.txt", &context()).await.err().unwrap();
    assert_eq!((error.code, error.path), (FileErrorCode::NotFound, Some(join(dir.path(), "missing.txt"))));
}
