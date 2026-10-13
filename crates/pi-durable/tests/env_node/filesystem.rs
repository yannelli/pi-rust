//! `NodeExecutionEnv filesystem` cases.

use pi_durable::env::{FileErrorCode, FileKind, FileSystem, RemoveOptions, TempFileOptions};

use crate::support::{aborted_context, context, join, local_env, path_string, remove_parent, temp_dir};

#[tokio::test]
async fn reads_writes_lists_and_removes_files_and_directories() {
    let dir = temp_dir();
    let root = dir.path();
    let env = local_env(root);
    let ctx = context();
    assert_eq!(env.absolute_path("nested/child", &ctx).await.unwrap(), join(root, "nested/child"));
    let parts = [path_string(root), "nested".into(), "child".into()];
    assert_eq!(env.join_path(&parts, &ctx).await.unwrap(), join(root, "nested/child"));
    env.create_dir("nested/child", true, &ctx).await.unwrap();
    env.write_file("nested/child/file.txt", b"hel", &ctx).await.unwrap();
    env.append_file("nested/child/file.txt", b"lo", &ctx).await.unwrap();
    assert_eq!(env.read_text_file("nested/child/file.txt", &ctx).await.unwrap(), "hello");
    assert_eq!(env.read_text_lines("nested/child/file.txt", Some(1), &ctx).await.unwrap(), ["hello"]);
    assert_eq!(env.read_binary_file("nested/child/file.txt", &ctx).await.unwrap(), b"hello");

    let entries = env.list_dir("nested/child", &ctx).await.unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "file.txt");
    assert_eq!(entries[0].path, join(root, "nested/child/file.txt"));
    assert_eq!((entries[0].kind, entries[0].size), (FileKind::File, 5));
    assert!(entries[0].mtime_ms > 0.0);

    assert!(env.exists("nested/child/file.txt", &ctx).await.unwrap());
    env.remove("nested/child/file.txt", RemoveOptions::default(), &ctx).await.unwrap();
    assert!(!env.exists("nested/child/file.txt", &ctx).await.unwrap());
}

#[tokio::test]
async fn expands_home_relative_paths_and_file_urls() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).unwrap();
    let expected = path_string(&std::path::Path::new(&home).join("pi-node-env-test"));
    assert_eq!(env.absolute_path("~/pi-node-env-test", &context()).await.unwrap(), expected);
    let file = dir.path().join("file with spaces.txt");
    let url = url::Url::from_file_path(&file).unwrap().to_string();
    assert!(url.contains("%20"));
    assert_eq!(env.absolute_path(&url, &context()).await.unwrap(), path_string(&file));
}

#[cfg(unix)]
#[tokio::test]
async fn returns_file_info_for_files_directories_and_symlinks_without_following_symlinks() {
    let dir = temp_dir();
    let root = dir.path();
    let env = local_env(root);
    let ctx = context();
    env.create_dir("dir", true, &ctx).await.unwrap();
    env.write_file("dir/file.txt", b"hello", &ctx).await.unwrap();
    std::os::unix::fs::symlink(root.join("dir/file.txt"), root.join("file-link")).unwrap();
    std::os::unix::fs::symlink(root.join("dir"), root.join("dir-link")).unwrap();

    let info = env.file_info("dir", &ctx).await.unwrap();
    assert_eq!((info.name.as_str(), info.path, info.kind), ("dir", join(root, "dir"), FileKind::Directory));
    let info = env.file_info("dir/file.txt", &ctx).await.unwrap();
    assert_eq!((info.name.as_str(), info.kind, info.size), ("file.txt", FileKind::File, 5));
    assert_eq!(info.path, join(root, "dir/file.txt"));
    let info = env.file_info("file-link", &ctx).await.unwrap();
    assert_eq!((info.name.as_str(), info.path, info.kind), ("file-link", join(root, "file-link"), FileKind::Symlink));
    let info = env.file_info("dir-link", &ctx).await.unwrap();
    assert_eq!((info.name.as_str(), info.path, info.kind), ("dir-link", join(root, "dir-link"), FileKind::Symlink));
    let canonical = path_string(&std::fs::canonicalize(root.join("dir/file.txt")).unwrap());
    assert_eq!(env.canonical_path("file-link", &ctx).await.unwrap(), canonical);
}

#[cfg(unix)]
#[tokio::test]
async fn lists_symlinks_as_symlinks() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    env.write_file("target.txt", b"hello", &context()).await.unwrap();
    std::os::unix::fs::symlink(dir.path().join("target.txt"), dir.path().join("link.txt")).unwrap();
    let mut entries: Vec<_> =
        env.list_dir(".", &context()).await.unwrap().into_iter().map(|entry| (entry.name, entry.kind)).collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(entries, [("link.txt".into(), FileKind::Symlink), ("target.txt".into(), FileKind::File)]);
}

#[tokio::test]
async fn stops_reading_text_lines_at_the_requested_limit() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    env.write_file("file.txt", b"one\ntwo\nthree", &context()).await.unwrap();
    assert_eq!(env.read_text_lines("file.txt", Some(1), &context()).await.unwrap(), ["one"]);
}

#[tokio::test]
async fn returns_file_error_for_missing_paths_and_keeps_exists_false() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let error = env.file_info("missing.txt", &context()).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);
    assert_eq!(error.path, Some(join(dir.path(), "missing.txt")));
    assert!(!env.exists("missing.txt", &context()).await.unwrap());
}

#[tokio::test]
async fn returns_file_error_for_listing_non_directories() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    env.write_file("file.txt", b"hello", &context()).await.unwrap();
    let error = env.list_dir("file.txt", &context()).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotDirectory);
}

#[tokio::test]
async fn appends_to_new_files_and_creates_parent_directories() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    env.append_file("new/nested/file.txt", b"a", &context()).await.unwrap();
    env.append_file("new/nested/file.txt", b"b", &context()).await.unwrap();
    assert_eq!(env.read_text_file("new/nested/file.txt", &context()).await.unwrap(), "ab");
}

#[tokio::test]
async fn atomically_renames_a_file_and_replaces_the_destination() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("source.txt", b"new", &ctx).await.unwrap();
    env.write_file("destination.txt", b"old", &ctx).await.unwrap();
    env.rename_file("source.txt", "destination.txt", &ctx).await.unwrap();
    assert!(!env.exists("source.txt", &ctx).await.unwrap());
    assert_eq!(env.read_text_file("destination.txt", &ctx).await.unwrap(), "new");
}

#[tokio::test]
async fn reports_the_source_path_when_rename_fails_because_the_source_is_missing() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("destination.txt", b"unchanged", &ctx).await.unwrap();
    let error = env.rename_file("missing-source.txt", "destination.txt", &ctx).await.unwrap_err();
    assert_eq!((error.code, error.path), (FileErrorCode::NotFound, Some(join(dir.path(), "missing-source.txt"))));
    assert_eq!(env.read_text_file("destination.txt", &ctx).await.unwrap(), "unchanged");
}

#[tokio::test]
async fn creates_temporary_directories_and_files() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    let temp = env.create_temp_dir(Some("node-env-test-"), &ctx).await.unwrap();
    assert!(std::path::Path::new(&temp).is_dir());
    std::fs::remove_dir_all(&temp).unwrap();
    let options = TempFileOptions { prefix: Some("prefix-".into()), suffix: Some(".txt".into()) };
    let file = env.create_temp_file(options, &ctx).await.unwrap();
    assert!(std::path::Path::new(&file).is_file());
    assert!(file.ends_with(".txt"));
    assert_eq!(env.read_text_file(&file, &ctx).await.unwrap(), "");
    remove_parent(&file);
}

#[tokio::test]
async fn honors_create_dir_recursive_false_and_remove_recursive_and_force_options() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    let error = env.create_dir("missing/child", false, &ctx).await.unwrap_err();
    assert_eq!(error.code, FileErrorCode::NotFound);

    env.write_file("dir/child/file.txt", b"hello", &ctx).await.unwrap();
    assert!(env.remove("dir", RemoveOptions { recursive: false, force: false }, &ctx).await.is_err());
    env.remove("dir", RemoveOptions { recursive: true, force: false }, &ctx).await.unwrap();
    assert!(!env.exists("dir", &ctx).await.unwrap());

    assert!(env.remove("missing", RemoveOptions { recursive: false, force: false }, &ctx).await.is_err());
    env.remove("missing", RemoveOptions { recursive: false, force: true }, &ctx).await.unwrap();
}

#[tokio::test]
async fn returns_aborted_results_without_side_effects_for_pre_aborted_file_operations() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    env.write_file("file.txt", b"hello", &context()).await.unwrap();
    let ctx = aborted_context();
    let codes = [
        env.read_text_file("file.txt", &ctx).await.err(),
        env.read_text_lines("file.txt", None, &ctx).await.err(),
        env.read_binary_file("file.txt", &ctx).await.err(),
        env.open_text_line_reader("file.txt", &ctx).await.err(),
        env.write_file("other.txt", b"hello", &ctx).await.err(),
        env.append_file("file.txt", b" world", &ctx).await.err(),
        env.truncate_file("file.txt", 1, &ctx).await.err(),
        env.flush_file("file.txt", &ctx).await.err(),
        env.rename_file("file.txt", "renamed.txt", &ctx).await.err(),
        env.file_info("file.txt", &ctx).await.err(),
        env.list_dir(".", &ctx).await.err(),
        env.canonical_path("file.txt", &ctx).await.err(),
        env.exists("file.txt", &ctx).await.err(),
        env.create_dir("dir", true, &ctx).await.err(),
        env.remove("file.txt", RemoveOptions::default(), &ctx).await.err(),
        env.create_temp_dir(None, &ctx).await.err(),
        env.create_temp_file(TempFileOptions::default(), &ctx).await.err(),
    ];
    for error in codes {
        assert_eq!(error.map(|error| error.code), Some(FileErrorCode::Aborted));
    }
    assert_eq!(env.read_text_file("file.txt", &context()).await.unwrap(), "hello");
    let names: Vec<String> = env.list_dir(".", &context()).await.unwrap().into_iter().map(|entry| entry.name).collect();
    assert_eq!(names, ["file.txt"]);
}

#[tokio::test]
async fn truncates_and_extends_files_to_exact_byte_sizes() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("file.bin", &[0x61, 0xc3, 0xa9, 0x0a, 0x62, 0x0a], &ctx).await.unwrap();
    // Truncation is byte-exact even when the boundary splits a UTF-8 sequence.
    env.truncate_file("file.bin", 2, &ctx).await.unwrap();
    assert_eq!(env.read_binary_file("file.bin", &ctx).await.unwrap(), [0x61, 0xc3]);
    env.truncate_file("file.bin", 4, &ctx).await.unwrap();
    assert_eq!(env.read_binary_file("file.bin", &ctx).await.unwrap(), [0x61, 0xc3, 0, 0]);
    env.truncate_file("file.bin", 0, &ctx).await.unwrap();
    assert_eq!(env.file_info("file.bin", &ctx).await.unwrap().size, 0);
}

/// Negative, fractional, and non-finite sizes cannot be expressed as `u64`; only unsafe integers remain to reject.
#[tokio::test]
async fn rejects_invalid_truncation_sizes_and_never_creates_missing_files() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("file.txt", b"hello", &ctx).await.unwrap();
    for size in [(1u64 << 53), u64::MAX] {
        let error = env.truncate_file("file.txt", size, &ctx).await.unwrap_err();
        assert_eq!((error.code, error.path), (FileErrorCode::Invalid, Some(join(dir.path(), "file.txt"))));
    }
    assert_eq!(env.read_text_file("file.txt", &ctx).await.unwrap(), "hello");
    let error = env.truncate_file("missing.txt", 0, &ctx).await.unwrap_err();
    assert_eq!((error.code, error.path), (FileErrorCode::NotFound, Some(join(dir.path(), "missing.txt"))));
    assert!(!env.exists("missing.txt", &ctx).await.unwrap());
}

#[tokio::test]
async fn flushes_existing_files_without_changing_content_and_reports_missing_paths() {
    let dir = temp_dir();
    let env = local_env(dir.path());
    let ctx = context();
    env.write_file("file.txt", b"durable", &ctx).await.unwrap();
    env.flush_file("file.txt", &ctx).await.unwrap();
    assert_eq!(env.read_text_file("file.txt", &ctx).await.unwrap(), "durable");
    let error = env.flush_file("missing.txt", &ctx).await.unwrap_err();
    assert_eq!((error.code, error.path), (FileErrorCode::NotFound, Some(join(dir.path(), "missing.txt"))));
    assert!(!env.exists("missing.txt", &ctx).await.unwrap());
    env.create_dir("dir", true, &ctx).await.unwrap();
    assert_eq!(env.flush_file("dir", &ctx).await.unwrap_err().code, FileErrorCode::IsDirectory);
}

// "syncs through an opened handle, returns sync failures, and always closes the handle" spies on Node's
// `FileHandle.prototype.sync`; Rust has no seam to inject an fsync failure, so the case does not apply. The flush
// case above covers the successful sync.

#[tokio::test]
async fn cleanup_is_best_effort() {
    let dir = temp_dir();
    local_env(dir.path()).cleanup(&context()).await;
}
