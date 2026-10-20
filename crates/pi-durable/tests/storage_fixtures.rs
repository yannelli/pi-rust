//! Byte compatibility with upstream: TypeScript wrote `tests/fixtures/storage` (see its `generate.ts`, a
//! development-time script these tests never run). Rust reads that data back, and the same commits made from
//! Rust produce identical JSONL bytes and identical SQLite rows.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pi_chord::{BACKGROUND_CONTEXT, CopyJsonOptions, Json, JsonMap, copy_json};
use pi_durable::env::{LocalEnvOptions, LocalExecutionEnv};
use pi_durable::storage::jsonl::{JsonlStorage, JsonlStorageOptions};
use pi_durable::storage::sqlite::open_sqlite_storage;
use pi_durable::*;
use rusqlite::Connection;
use rusqlite::types::ValueRef;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/storage")
}

fn ctx() -> &'static pi_chord::Context {
    &BACKGROUND_CONTEXT
}

fn to_json<T: serde::Serialize>(value: &T) -> Json {
    copy_json(value, CopyJsonOptions { omit_undefined_properties: true }).unwrap()
}

fn object(entries: Vec<(&str, Json)>) -> Json {
    let mut map = JsonMap::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Json::object(map)
}

fn stored_entry(stored: Option<StoredEntry>) -> Json {
    stored.map_or(Json::Null, |stored| {
        object(vec![("entry", to_json(&stored.entry)), ("commitSeq", Json::from(stored.commit_seq.0))])
    })
}

/// The reads `generate.ts` records in `expected.json`.
async fn reads(storage: &dyn Storage) -> Json {
    let conversations = [ConversationId(1), ConversationId(10)];
    let mut found = Vec::new();
    for id in conversations {
        found.push(to_json(&storage.conversation(id, ctx()).await.unwrap()));
    }
    let mut entries = Vec::new();
    for id in [2, 3, 13] {
        entries.push(stored_entry(storage.entry(EntryId(id), ctx()).await.unwrap()));
    }
    let visible = storage.scan_entries(&EntryQuery::new(ConversationId(10)), 100, None, ctx()).await.unwrap().items;
    let tasks = [
        to_json(&storage.task(TaskId(5), ctx()).await.unwrap()),
        to_json(&storage.task(TaskId(14), ctx()).await.unwrap()),
    ];
    let submissions = [
        to_json(&storage.submission(SubmissionId(6), ctx()).await.unwrap()),
        to_json(&storage.submission(SubmissionId(15), ctx()).await.unwrap()),
    ];
    let mut documents = Vec::new();
    for id in [7, 8, 9, 11, 12] {
        let points = [DocumentPoint::Current].into_iter().chain((3..=8).map(|seq| DocumentPoint::At(Seq(seq))));
        for at in points {
            let stored = match storage.document(DocumentId(id), at, ctx()).await {
                Ok(None) => Json::Null,
                Ok(Some(stored)) => object(vec![
                    ("record", to_json(&stored.record)),
                    ("version", Json::from(stored.version)),
                    ("value", stored.value),
                    ("deltasSinceBase", Json::from(stored.deltas_since_base)),
                ]),
                Err(error) => object(vec![("error", Json::from(error.to_string()))]),
            };
            documents.push(object(vec![("id", Json::from(id)), ("at", to_json(&at)), ("stored", stored)]));
        }
    }
    object(vec![
        ("conversations", Json::array(found)),
        ("entries", Json::array(entries)),
        ("visible", to_json(&visible)),
        ("tasks", Json::array(tasks.to_vec())),
        ("submissions", Json::array(submissions.to_vec())),
        ("documents", Json::array(documents)),
    ])
}

fn expected() -> Json {
    Json::parse(&std::fs::read_to_string(fixtures().join("expected.json")).unwrap()).unwrap()
}

fn batches() -> Vec<Vec<StorageWrite>> {
    let text = std::fs::read_to_string(fixtures().join("commits.json")).unwrap();
    Json::parse(&text).unwrap().deserialize_into().unwrap()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

async fn open_jsonl(directory: &Path) -> JsonlStorage {
    let directory = directory.to_string_lossy().into_owned();
    let fs = Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(directory.clone())));
    JsonlStorage::open(&directory, fs, ctx(), JsonlStorageOptions::default()).await.unwrap()
}

fn assert_same_reads(actual: Json, expected: Json) {
    assert!(actual == expected, "reads differ:\n{}\n---\n{}", actual.to_string_pretty(1), expected.to_string_pretty(1));
}

#[tokio::test]
async fn reads_the_sqlite_file_typescript_wrote() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite");
    std::fs::copy(fixtures().join("storage.sqlite"), &path).unwrap();
    let storage = open_sqlite_storage(&path.to_string_lossy()).await.unwrap();
    assert_same_reads(reads(&storage).await, expected());
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn reads_the_jsonl_directory_typescript_wrote() {
    let directory = tempfile::tempdir().unwrap();
    copy_dir(&fixtures().join("jsonl"), directory.path());
    let storage = open_jsonl(directory.path()).await;
    assert_same_reads(reads(&storage).await, expected());
    storage.close(ctx()).await.unwrap();
}

fn file_names(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn writes_the_same_jsonl_bytes_as_typescript() {
    let directory = tempfile::tempdir().unwrap();
    let storage = open_jsonl(directory.path()).await;
    for batch in batches() {
        storage.commit(&batch, ctx()).await.unwrap();
    }
    storage.close(ctx()).await.unwrap();
    let golden = fixtures().join("jsonl");
    assert_eq!(file_names(directory.path()), file_names(&golden));
    for name in file_names(&golden) {
        let actual = std::fs::read_to_string(directory.path().join(&name)).unwrap();
        let expected = std::fs::read_to_string(golden.join(&name)).unwrap();
        assert_eq!(actual, expected, "{name}");
    }
}

const TABLES: [(&str, &str); 9] = [
    ("durable_schema", "singleton"),
    ("durable_metadata", "singleton"),
    ("record_ids", "id"),
    ("conversations", "id"),
    ("entries", "id"),
    ("tasks", "id"),
    ("submissions", "id"),
    ("documents", "id"),
    ("document_revisions", "document_id, seq"),
];

/// Every row of every storage table, as debug text with SQLite's own value types.
fn rows(path: &Path) -> Vec<String> {
    let connection = Connection::open(path).unwrap();
    let mut rows = Vec::new();
    for (table, order) in TABLES {
        let mut statement = connection.prepare(&format!("SELECT * FROM {table} ORDER BY {order}")).unwrap();
        let columns = statement.column_count();
        let mut query = statement.query([]).unwrap();
        while let Some(row) = query.next().unwrap() {
            let values: Vec<String> = (0..columns)
                .map(|index| match row.get_ref(index).unwrap() {
                    ValueRef::Text(text) => format!("text:{}", String::from_utf8_lossy(text)),
                    other => format!("{other:?}"),
                })
                .collect();
            rows.push(format!("{table}: {}", values.join(" | ")));
        }
    }
    rows
}

#[tokio::test]
async fn writes_the_same_sqlite_rows_as_typescript() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite");
    let storage = open_sqlite_storage(&path.to_string_lossy()).await.unwrap();
    for batch in batches() {
        storage.commit(&batch, ctx()).await.unwrap();
    }
    storage.close(ctx()).await.unwrap();
    let golden = tempfile::tempdir().unwrap();
    let golden_path = golden.path().join("storage.sqlite");
    std::fs::copy(fixtures().join("storage.sqlite"), &golden_path).unwrap();
    // The fixture predates schema version 2; opening it applies the same migration TypeScript does.
    open_sqlite_storage(&golden_path.to_string_lossy()).await.unwrap().close(ctx()).await.unwrap();
    assert_eq!(rows(&path), rows(&golden_path));
}
