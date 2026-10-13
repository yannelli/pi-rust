//! Binary reader and directory reader cases.

use super::{EnvConformanceOptions, aborted_context, context, shell_argv};
use crate::env::{FileErrorCode, FileInfo, FileKind, LineRange, RemoveOptions, SharedEnv};

fn utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decoding `bytes` whole like `TextDecoder`, which drops a leading byte-order mark only when `strip_bom`.
fn decode(bytes: &[u8], strip_bom: bool) -> String {
    let text = utf8(bytes);
    match text.strip_prefix('\u{feff}') {
        Some(rest) if strip_bom => rest.to_string(),
        _ => text,
    }
}

async fn read_all(env: &SharedEnv, path: &str, max_entries: usize) -> (Vec<Vec<FileInfo>>, bool) {
    let reader = env.open_dir_reader(path, &context()).await.expect("open directory reader");
    let mut pages = Vec::new();
    let mut done = false;
    for _ in 0..1000 {
        let page = reader.next(max_entries, &context()).await.expect("next page");
        pages.push(page.entries);
        if page.done {
            done = true;
            break;
        }
    }
    reader.close(&context()).await;
    (pages, done)
}

/// Offsets beyond `2^53 - 1` stand in for upstream's negative and fractional offsets, which `u64` cannot hold.
pub async fn binary_reader_reads_byte_ranges_of_the_opened_file(env: SharedEnv, _options: &EnvConformanceOptions) {
    let ctx = context();
    env.write_file("data.txt", b"hello world", &ctx).await.unwrap();
    let reader = env.open_binary_reader("data.txt", false, &ctx).await.unwrap();
    let info = reader.info(&ctx).await.unwrap();
    assert_eq!((info.name.as_str(), info.kind, info.size), ("data.txt", FileKind::File, 11));
    assert_eq!(utf8(&reader.read(0, 5, &ctx).await.unwrap()), "hello");
    assert_eq!(utf8(&reader.read(6, 100, &ctx).await.unwrap()), "world");
    assert!(reader.read(11, 4, &ctx).await.unwrap().is_empty());
    assert!(reader.read(50, 1, &ctx).await.unwrap().is_empty());
    assert!(reader.read(3, 0, &ctx).await.unwrap().is_empty());
    assert_eq!(reader.read(u64::MAX, 1, &ctx).await.unwrap_err().code, FileErrorCode::Invalid);
    assert_eq!(reader.read(0, 1 << 53, &ctx).await.unwrap_err().code, FileErrorCode::Invalid);
    assert_eq!(reader.read(0, 1, &aborted_context()).await.unwrap_err().code, FileErrorCode::Aborted);
    reader.close(&ctx).await;
    reader.close(&ctx).await;
    assert_eq!(reader.read(0, 1, &ctx).await.unwrap_err().code, FileErrorCode::Invalid);
    assert_eq!(reader.info(&ctx).await.unwrap_err().code, FileErrorCode::Invalid);
}

pub async fn binary_reader_scans_lines_like_decoding_the_whole_file(env: SharedEnv, _options: &EnvConformanceOptions) {
    let ctx = context();
    // A byte-order mark, an invalid sequence before a newline, an empty line, a later U+FEFF, and no final newline.
    let bytes = [0xef, 0xbb, 0xbf, 0x61, 0x0a, 0xe2, 0x82, 0x0a, 0x0a, 0xef, 0xbb, 0xbf, 0x62, 0x0a, 0xc3, 0xa9];
    env.write_file("lines.txt", &bytes, &ctx).await.unwrap();
    let whole = decode(&bytes, true);
    let lines: Vec<&str> = whole.split('\n').collect();
    let reader = env.open_binary_reader("lines.txt", false, &ctx).await.unwrap();
    let ranges = [(0, None), (0, Some(1)), (1, Some(3)), (2, Some(3)), (3, None), (4, Some(9))];
    for (start_line, end_line) in ranges {
        let scan = reader.scan_lines(LineRange { start_line, end_line }, &ctx).await.unwrap();
        let end = end_line.map_or(lines.len(), |end| (end as usize).min(lines.len()));
        let selected = lines[start_line as usize..end].join("\n");
        let range = |from: u64, to: u64| decode(&bytes[from as usize..to as usize], from == 0);
        assert_eq!(scan.newlines as usize, lines.len() - 1);
        assert_eq!(range(scan.start, scan.end), selected);
        assert_eq!(scan.selected_bytes as usize, selected.len());
        assert_eq!(range(scan.start, scan.first_line_end), lines[start_line as usize]);
        assert_eq!(scan.first_line_bytes as usize, lines[start_line as usize].len());
    }
    let past = reader.scan_lines(LineRange { start_line: 9, end_line: None }, &ctx).await.unwrap();
    let size = bytes.len() as u64;
    assert_eq!((past.start, past.end, past.selected_bytes), (size, size, 0));
    let empty = reader.scan_lines(LineRange { start_line: 2, end_line: Some(2) }, &ctx).await;
    assert_eq!(empty.unwrap_err().code, FileErrorCode::Invalid);
    reader.close(&ctx).await;
}

pub async fn binary_reader_keeps_reading_the_file_it_opened_after_a_rename(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let ctx = context();
    env.write_file("a.txt", b"one", &ctx).await.unwrap();
    let reader = env.open_binary_reader("a.txt", false, &ctx).await.unwrap();
    env.rename_file("a.txt", "b.txt", &ctx).await.unwrap();
    env.write_file("a.txt", b"two", &ctx).await.unwrap();
    assert_eq!(utf8(&reader.read(0, 10, &ctx).await.unwrap()), "one");
    reader.close(&ctx).await;
}

pub async fn binary_reader_refuses_directories_missing_files_and_aborted_opens(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let ctx = context();
    env.create_dir("dir", true, &ctx).await.unwrap();
    env.write_file("file.txt", b"x", &ctx).await.unwrap();
    let code = |result: Result<_, crate::env::FileError>| result.err().map(|error| error.code);
    assert_eq!(code(env.open_binary_reader("dir", false, &ctx).await), Some(FileErrorCode::IsDirectory));
    assert_eq!(code(env.open_binary_reader("missing.txt", false, &ctx).await), Some(FileErrorCode::NotFound));
    let aborted = env.open_binary_reader("file.txt", false, &aborted_context()).await;
    assert_eq!(code(aborted), Some(FileErrorCode::Aborted));
}

pub async fn directory_reader_pages_every_entry_exactly_once(env: SharedEnv, _options: &EnvConformanceOptions) {
    let ctx = context();
    let names = ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"];
    for name in names {
        env.write_file(name, name.as_bytes(), &ctx).await.unwrap();
    }
    env.create_dir("sub", true, &ctx).await.unwrap();
    let (pages, done) = read_all(&env, ".", 2).await;
    assert!(done, "directory reader reached the end");
    assert!(pages.iter().all(|page| page.len() <= 2), "page within maxEntries");
    let entries: Vec<FileInfo> = pages.into_iter().flatten().collect();
    let mut seen: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    seen.sort_unstable();
    assert_eq!(seen, ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "sub"]);
    let find = |name: &str| entries.iter().find(|entry| entry.name == name).unwrap();
    assert_eq!(find("sub").kind, FileKind::Directory);
    assert_eq!((find("a.txt").kind, find("a.txt").size), (FileKind::File, 5));
}

pub async fn directory_reader_reports_the_end_and_refuses_use_after_close(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let ctx = context();
    env.create_dir("empty", true, &ctx).await.unwrap();
    let reader = env.open_dir_reader("empty", &ctx).await.unwrap();
    for _ in 0..2 {
        let page = reader.next(10, &ctx).await.unwrap();
        assert!(page.entries.is_empty() && page.done);
    }
    assert_eq!(reader.next(0, &ctx).await.unwrap_err().code, FileErrorCode::Invalid);
    assert_eq!(reader.next(1, &aborted_context()).await.unwrap_err().code, FileErrorCode::Aborted);
    reader.close(&ctx).await;
    reader.close(&ctx).await;
    assert_eq!(reader.next(1, &ctx).await.unwrap_err().code, FileErrorCode::Invalid);
}

pub async fn directory_reader_refuses_missing_paths_and_files(env: SharedEnv, _options: &EnvConformanceOptions) {
    let ctx = context();
    env.write_file("file.txt", b"x", &ctx).await.unwrap();
    let code = |result: Result<_, crate::env::FileError>| result.err().map(|error| error.code);
    assert_eq!(code(env.open_dir_reader("missing", &ctx).await), Some(FileErrorCode::NotFound));
    assert_eq!(code(env.open_dir_reader("file.txt", &ctx).await), Some(FileErrorCode::NotDirectory));
    assert_eq!(code(env.open_dir_reader(".", &aborted_context()).await), Some(FileErrorCode::Aborted));
}

pub async fn directory_reader_skips_entries_removed_during_enumeration(
    env: SharedEnv,
    _options: &EnvConformanceOptions,
) {
    let ctx = context();
    env.create_dir("dir", true, &ctx).await.unwrap();
    for name in ["x", "y", "z"] {
        env.write_file(&format!("dir/{name}"), name.as_bytes(), &ctx).await.unwrap();
    }
    let reader = env.open_dir_reader("dir", &ctx).await.unwrap();
    for name in ["x", "y", "z"] {
        env.remove(&format!("dir/{name}"), RemoveOptions::default(), &ctx).await.unwrap();
    }
    let mut entries = Vec::new();
    for _ in 0..10 {
        let page = reader.next(10, &ctx).await.unwrap();
        entries.extend(page.entries);
        if page.done {
            break;
        }
    }
    assert!(entries.is_empty(), "{entries:?}");
    reader.close(&ctx).await;
}

pub async fn binary_reader_follows_symlinks_unless_no_follow_refuses_the_final_one(
    env: SharedEnv,
    options: &EnvConformanceOptions,
) {
    if !options.symlinks {
        return;
    }
    let ctx = context();
    env.write_file("target.txt", b"target", &ctx).await.unwrap();
    env.create_dir("sub", true, &ctx).await.unwrap();
    env.write_file("sub/inner.txt", b"inner", &ctx).await.unwrap();
    let command = shell_argv(options, &["ln -s target.txt link.txt && ln -s sub dirlink"]);
    let linked = env.exec(command, Default::default(), &ctx).await;
    assert_eq!(linked.unwrap().exit_code, 0);

    let followed = env.open_binary_reader("link.txt", false, &ctx).await.unwrap();
    assert_eq!(utf8(&followed.read(0, 10, &ctx).await.unwrap()), "target");
    followed.close(&ctx).await;
    let refused = env.open_binary_reader("link.txt", true, &ctx).await;
    assert_eq!(refused.err().map(|error| error.code), Some(FileErrorCode::Invalid));
    // Only the final component is refused; earlier symlinked directories still resolve.
    let inner = env.open_binary_reader("dirlink/inner.txt", true, &ctx).await.unwrap();
    assert_eq!(utf8(&inner.read(0, 10, &ctx).await.unwrap()), "inner");
    inner.close(&ctx).await;
}
