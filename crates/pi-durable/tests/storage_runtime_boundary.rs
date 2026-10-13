//! Port of upstream `test/storage-runtime-boundary.test.ts`. Upstream walks the import graph for Node built-ins;
//! here the portable modules must not name the local file system, a SQLite binding, or the local environment.

use std::path::{Path, PathBuf};

fn source(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(relative)
}

/// Every `.rs` file at or below `relative`.
fn files(relative: &str) -> Vec<PathBuf> {
    let path = source(relative);
    if path.is_file() {
        return vec![path];
    }
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&path).unwrap() {
        let entry = entry.unwrap().path();
        if entry.is_dir() {
            found.extend(files(&entry.strip_prefix(source("")).unwrap().to_string_lossy()));
        } else if entry.extension().is_some_and(|extension| extension == "rs") {
            found.push(entry);
        }
    }
    found
}

fn assert_free_of(paths: &[PathBuf], forbidden: &[&str]) {
    assert!(!paths.is_empty());
    for path in paths {
        let text = std::fs::read_to_string(path).unwrap();
        for name in forbidden {
            assert!(!text.contains(name), "{} names {name}", path.display());
        }
    }
}

const HOST: [&str; 4] = ["std::fs", "tokio::fs", "rusqlite", "LocalExecutionEnv"];

#[test]
fn keeps_the_core_and_memory_storage_free_of_backends() {
    let mut core = Vec::new();
    for relative in ["types", "ids.rs", "errors.rs", "storage/memory", "storage/scan.rs"] {
        core.extend(files(relative));
    }
    let mut forbidden = HOST.to_vec();
    forbidden.extend(["crate::env", "storage::sqlite", "storage::jsonl"]);
    assert_free_of(&core, &forbidden);
}

#[test]
fn keeps_the_portable_sqlite_storage_free_of_its_bundled_binding() {
    let portable: Vec<PathBuf> = files("storage/sqlite")
        .into_iter()
        .filter(|path| !path.ends_with("connection.rs") && !path.ends_with("mod.rs"))
        .collect();
    assert_free_of(&portable, &HOST);
    let root = std::fs::read_to_string(source("storage/sqlite/mod.rs")).unwrap();
    assert!(!root.contains("rusqlite") && !root.contains("std::fs"), "the SQLite root only re-exports the opener");
}

// "keeps the portable environment subpath free of Node imports" belongs to the env port: `env::local` holds the local
// implementation and `env/{mod,types}.rs` only declare the portable traits.

#[test]
fn keeps_the_portable_jsonl_storage_free_of_the_local_environment() {
    let portable: Vec<PathBuf> =
        files("storage/jsonl").into_iter().filter(|path| !path.ends_with("local.rs")).collect();
    assert_free_of(&portable, &HOST);
}
