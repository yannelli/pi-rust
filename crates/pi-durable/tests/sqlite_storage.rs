//! Port of upstream `test/sqlite-storage.test.ts`.

mod support;

use std::sync::Arc;

use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_durable::storage::sqlite::{SqliteStorage, SqliteStorageOptions, open_sqlite_storage, open_sqlite_storage_with};
use pi_durable::testing::storage_conformance::StorageTest;
use pi_durable::*;
use rusqlite::Connection;
use support::reopening::{Reopen, ReopeningStorage};

fn ctx() -> &'static pi_chord::Context {
    &BACKGROUND_CONTEXT
}

fn temp_path() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite").to_string_lossy().into_owned();
    (directory, path)
}

async fn with_sqlite_storage(test: StorageTest) {
    let (_directory, path) = temp_path();
    let storage = Arc::new(open_sqlite_storage(&path).await.unwrap());
    test(storage.clone()).await;
    let _ = storage.close(ctx()).await;
}

async fn with_reopening_sqlite_storage(test: StorageTest) {
    let (_directory, path) = temp_path();
    open_sqlite_storage(&path).await.unwrap().close(ctx()).await.unwrap();
    let reopen: Reopen = Arc::new(move || {
        let path = path.clone();
        Box::pin(async move { Arc::new(open_sqlite_storage(&path).await.unwrap()) as Arc<dyn Storage> })
    });
    let storage = Arc::new(ReopeningStorage::new(reopen).await);
    test(storage.clone()).await;
    storage.close(ctx()).await.unwrap();
}

mod conformance {
    pi_durable::storage_conformance_tests!(super::with_sqlite_storage);
}

mod conformance_across_reopen {
    pi_durable::storage_conformance_tests!(super::with_reopening_sqlite_storage);
}

fn prefixed() -> SqliteStorageOptions {
    SqliteStorageOptions { prefix: Some("pi_".into()), ..SqliteStorageOptions::default() }
}

/// Every storage query against prefixed tables, beside a host table that has an unprefixed storage name.
async fn with_prefixed_sqlite_storage(test: StorageTest) {
    let (_directory, path) = temp_path();
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE tasks (text TEXT)").unwrap();
    let storage = Arc::new(open_sqlite_storage_with(&path, prefixed()).await.unwrap());
    test(storage.clone()).await;
    let _ = storage.close(ctx()).await;
}

mod conformance_with_prefix {
    pi_durable::storage_conformance_tests!(super::with_prefixed_sqlite_storage);
}

fn json(text: &str) -> Json {
    Json::parse(text).unwrap()
}

fn entry(id: EntryId, data: Option<Json>) -> EntryRecord {
    EntryRecord {
        kind: "message".into(),
        model: None,
        data,
        edits: None,
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        head: None,
        by_task_id: None,
    }
}

async fn create_root(storage: &SqliteStorage) -> Seq {
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: root }], &|_| {}, ctx()).await.unwrap()
}

fn pending_task(id: TaskId) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "test.task".into(),
        version: 1,
        input: Json::Null,
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: json(r#"{"phase":"ready"}"#) },
        memos: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        trace: Default::default(),
        links: Default::default(),
    }
}

fn session_document(id: DocumentId, kind: &str) -> DocumentCreate {
    DocumentCreate { id, kind: kind.into(), key: None, scope: DocumentScope::Session, history: None, fork: None }
}

fn create(record: DocumentCreate, value: Json) -> StorageWrite {
    StorageWrite::DocumentCreate { record, content: DocumentContent::Base { version: 1, value } }
}

fn change(id: DocumentId, content: DocumentContent) -> StorageWrite {
    StorageWrite::DocumentChange { id, content }
}

fn delta(ops: &str) -> DocumentContent {
    DocumentContent::Delta { version: 1, ops: pi_chord::delta::parse_ops(&json(ops)).unwrap() }
}

fn scalar(path: &str, sql: &str) -> i64 {
    let connection = Connection::open(path).unwrap();
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn revision_count(path: &str, id: DocumentId) -> i64 {
    scalar(path, &format!("SELECT count(*) FROM document_revisions WHERE document_id = {id}"))
}

#[tokio::test]
async fn persists_records_and_id_allocation_across_reopen() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    assert_eq!(create_root(&storage).await, Seq(1));
    let entry_id = EntryId(storage.mint_id().await.unwrap());
    let seq = storage.commit(&[StorageWrite::Entry { value: entry(entry_id, None) }], &|_| {}, ctx()).await.unwrap();
    assert_eq!(seq, Seq(2));
    storage.close(ctx()).await.unwrap();

    let reopened = open_sqlite_storage(&path).await.unwrap();
    let stored = reopened.entry(entry_id, ctx()).await.unwrap().unwrap();
    assert_eq!((stored.entry, stored.commit_seq), (entry(entry_id, None), Seq(2)));
    assert_eq!(reopened.mint_id().await.unwrap(), entry_id.0 + 1);
    let task = pending_task(TaskId(reopened.mint_id().await.unwrap()));
    assert_eq!(reopened.commit(&[StorageWrite::Task { value: task }], &|_| {}, ctx()).await.unwrap(), Seq(3));
    reopened.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn namespaces_tables_and_indexes_with_a_prefix_across_reopen() {
    let (_directory, path) = temp_path();
    Connection::open(&path).unwrap().execute_batch("CREATE TABLE tasks (text TEXT)").unwrap();
    let storage = open_sqlite_storage_with(&path, prefixed()).await.unwrap();
    assert_eq!(create_root(&storage).await, Seq(1));
    storage.close(ctx()).await.unwrap();

    let reopened = open_sqlite_storage_with(&path, prefixed()).await.unwrap();
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    assert_eq!(reopened.conversation(ROOT_CONVERSATION_ID, ctx()).await.unwrap(), Some(root));
    reopened.close(ctx()).await.unwrap();
    let connection = Connection::open(&path).unwrap();
    let mut names = connection
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let names: Vec<String> = names.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
    let unprefixed: Vec<&String> = names.iter().filter(|name| !name.starts_with("pi_")).collect();
    assert_eq!(unprefixed, ["tasks"]);
    assert!(names.iter().any(|name| name == "pi_tasks"));
    assert!(names.iter().any(|name| name == "pi_entry_heads_by_conversation"));
}

#[tokio::test]
async fn rejects_an_invalid_prefix_before_creating_the_database() {
    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("nested");
    let path = nested.join("storage.sqlite").to_string_lossy().into_owned();
    for (prefix, message) in [("bad-prefix", "Invalid SQLite schema prefix"), ("SQLite_x", "is reserved")] {
        let options = SqliteStorageOptions { prefix: Some(prefix.into()), ..SqliteStorageOptions::default() };
        let error = open_sqlite_storage_with(&path, options).await.err().expect("an invalid prefix");
        assert!(error.to_string().contains(message), "{error}");
    }
    assert!(!nested.exists());
}

#[tokio::test]
async fn rejects_persisted_metadata_corruption_on_reopen() {
    let (_directory, path) = temp_path();
    open_sqlite_storage(&path).await.unwrap().close(ctx()).await.unwrap();
    Connection::open(&path).unwrap().execute_batch("DELETE FROM durable_metadata").unwrap();
    let error = open_sqlite_storage(&path).await.err().expect("corrupt metadata");
    assert!(error.to_string().contains("Durable SQLite metadata is missing"), "{error}");
}

#[tokio::test]
async fn rejects_a_document_whose_required_base_is_missing() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let id = DocumentId(storage.mint_id().await.unwrap());
    storage
        .commit(&[create(session_document(id, "corrupt"), json(r#"{"retained":true}"#))], &|_| {}, ctx())
        .await
        .unwrap();
    let sql = format!("DELETE FROM document_revisions WHERE document_id = {id}");
    Connection::open(&path).unwrap().execute_batch(&sql).unwrap();
    let error = storage.document(id, DocumentPoint::Current, ctx()).await.unwrap_err();
    assert!(error.to_string().contains(&format!("Document {id} is missing a required base")), "{error}");
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn replays_root_replacements_and_rejects_corrupt_operations() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let id = DocumentId(storage.mint_id().await.unwrap());
    let initial = json(r#"{"nested":{"value":1},"rows":[]}"#);
    storage.commit(&[create(session_document(id, "replay"), initial)], &|_| {}, ctx()).await.unwrap();
    let edits = [
        r#"[["r",{"nested":{"value":2},"rows":[{"id":1}]}]]"#,
        r#"[["s",["nested","value"],3],["p",["rows"],1,0,[{"id":2}]],["m",["rows"],[1,0]]]"#,
        r#"[["s",["nested","value"],4]]"#,
    ];
    for ops in edits {
        storage.commit(&[change(id, delta(ops))], &|_| {}, ctx()).await.unwrap();
    }
    let expected = json(r#"{"nested":{"value":4},"rows":[{"id":2},{"id":1}]}"#);
    let mut first = storage.document(id, DocumentPoint::Current, ctx()).await.unwrap().unwrap();
    assert_eq!(first.value, expected);
    first.value.get_mut("nested").unwrap().set("value", 99).unwrap();
    let again = storage.document(id, DocumentPoint::Current, ctx()).await.unwrap().unwrap();
    assert_eq!(again.value, expected);

    let sql = "UPDATE document_revisions SET content = ?1 WHERE document_id = ?2 AND seq =
        (SELECT max(seq) FROM document_revisions WHERE document_id = ?2)";
    Connection::open(&path).unwrap().execute(sql, (r#"[["unknown"]]"#, id.0 as i64)).unwrap();
    let error = storage.document(id, DocumentPoint::Current, ctx()).await.unwrap_err();
    assert!(error.to_string().contains("unknown op verb"), "{error}");
    storage.close(ctx()).await.unwrap();
}

/// Upstream fails the batch with a circular task input, which Rust values cannot express. A document copy from an
/// unknown source fails at the same point: after the table rows of the batch were written.
#[tokio::test]
async fn rolls_sql_rows_and_sequence_allocation_back_as_one_transaction() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let transient_id = EntryId(storage.mint_id().await.unwrap());
    let transient_task = TaskId(storage.mint_id().await.unwrap());
    let copy_id = DocumentId(storage.mint_id().await.unwrap());
    let copy = StorageWrite::DocumentCopy {
        record: DocumentCreate {
            id: copy_id,
            kind: "copy".into(),
            key: None,
            scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
            history: Some(DocumentHistory::Latest),
            fork: Some(DocumentFork::Current),
        },
        source: DocumentCopySource { id: DocumentId(999_999), at: DocumentPoint::Current },
    };
    let writes = [
        StorageWrite::Entry { value: entry(transient_id, None) },
        StorageWrite::Task { value: pending_task(transient_task) },
        copy,
    ];
    let error = storage.commit(&writes, &|_| {}, ctx()).await.unwrap_err();
    assert!(error.to_string().contains("was rejected"), "{error}");
    assert_eq!(storage.entry(transient_id, ctx()).await.unwrap(), None);
    assert_eq!(storage.task(transient_task, ctx()).await.unwrap(), None);
    let committed = EntryId(storage.mint_id().await.unwrap());
    let seq = storage.commit(&[StorageWrite::Entry { value: entry(committed, None) }], &|_| {}, ctx()).await.unwrap();
    assert_eq!(seq, Seq(2));
    assert_eq!(scalar(&path, "SELECT count(*) FROM entries"), 1);
    assert_eq!(scalar(&path, "SELECT next_seq FROM durable_metadata WHERE singleton = 1"), 3);
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn reconstructs_recent_and_ancient_rewindable_points_after_reopen() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let id = DocumentId(storage.mint_id().await.unwrap());
    let record = DocumentCreate {
        id,
        kind: "history".into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    };
    let created_at = storage.commit(&[create(record, json(r#"{"count":0}"#))], &|_| {}, ctx()).await.unwrap();
    let (mut ancient_at, mut recent_at) = (created_at, created_at);
    for count in 1..=40 {
        let content = if count == 20 {
            DocumentContent::Base { version: 1, value: json(&format!(r#"{{"count":{count}}}"#)) }
        } else {
            delta(&format!(r#"[["s",["count"],{count}]]"#))
        };
        recent_at = storage.commit(&[change(id, content)], &|_| {}, ctx()).await.unwrap();
        if count == 5 {
            ancient_at = recent_at;
        }
    }
    storage.close(ctx()).await.unwrap();
    let reopened = open_sqlite_storage(&path).await.unwrap();
    let reopened = &reopened;
    let value = |at| async move { reopened.document(id, DocumentPoint::At(at), ctx()).await.unwrap().unwrap().value };
    assert_eq!(value(ancient_at).await, json(r#"{"count":5}"#));
    assert_eq!(value(recent_at).await, json(r#"{"count":40}"#));
}

#[path = "support/sqlite_files.rs"]
mod files;
