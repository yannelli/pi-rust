//! SQLite file-level cases of `sqlite-storage.test.ts`: query plans, reclamation, WAL, and size.

use rusqlite::{Connection, OpenFlags};

use super::*;

fn plan(connection: &Connection, sql: &str, params: impl rusqlite::Params) -> String {
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
    let rows = statement.query_map(params, |row| row.get::<_, String>("detail")).unwrap();
    rows.map(Result::unwrap).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn uses_indexes_for_addresses_scopes_history_and_revision_tails() {
    let (_directory, path) = temp_path();
    open_sqlite_storage(&path).await.unwrap().close(ctx()).await.unwrap();
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let details = [
        plan(
            &db,
            "SELECT record FROM documents
                WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
            ("kind", "session", 0, 0, ""),
        ),
        plan(
            &db,
            "SELECT record FROM documents
                WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                AND created_at <= ? AND (retired_at IS NULL OR retired_at > ?)
                ORDER BY created_at DESC LIMIT 1",
            ("kind", "conversation", 1, 0, "", 10, 10),
        ),
        plan(
            &db,
            "SELECT record FROM documents WHERE scope_kind = ? AND owner_id = ? AND kind = ? AND id > ? ORDER BY id LIMIT ?",
            ("task", 1, "kind", 0, 10),
        ),
        plan(
            &db,
            "SELECT record FROM entries WHERE conversation_id = ? AND id <= ? ORDER BY id DESC LIMIT ?",
            (1, 10, 10),
        ),
        plan(
            &db,
            "SELECT record FROM entries WHERE conversation_id = ? AND head IS NOT NULL AND id <= ? ORDER BY id DESC LIMIT 1",
            (1, 10),
        ),
        plan(&db, "SELECT record FROM tasks WHERE status = ? AND id > ? ORDER BY id LIMIT ?", ("pending", 0, 10)),
        plan(
            &db,
            "SELECT seq, kind, version, content FROM document_revisions
                WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1",
            (1, 10),
        ),
        plan(
            &db,
            "SELECT seq, kind, version, content FROM document_revisions
                WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq",
            (1, 5, 10),
        ),
    ];
    let expected = [
        "documents_by_address",
        "documents_by_address",
        "documents_by_scope_kind",
        "entries_by_conversation",
        "entry_heads_by_conversation",
        "tasks_by_status",
        "document_revisions_by_kind",
        "sqlite_autoindex_document_revisions_1",
    ];
    for (detail, index) in details.iter().zip(expected) {
        assert!(detail.contains(index), "{detail} should use {index}");
    }
    for detail in &details[..3] {
        assert!(!detail.contains("SCAN documents"), "{detail}");
    }
    assert!(!details[7].contains("SCAN document_revisions"), "{}", details[7]);
}

#[tokio::test]
async fn reclaims_current_only_revisions_only_after_a_base_or_retirement() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let id = DocumentId(storage.mint_id().await.unwrap());
    storage.commit(&[create(session_document(id, "latest"), json(r#"{"count":0}"#))], ctx()).await.unwrap();
    for count in 1..=10 {
        storage.commit(&[change(id, delta(&format!(r#"[["s",["count"],{count}]]"#)))], ctx()).await.unwrap();
    }
    assert_eq!(revision_count(&path, id), 11);
    let connection = Connection::open(&path).unwrap();
    let sql =
        "SELECT content FROM document_revisions WHERE document_id = ? AND kind = 'delta' ORDER BY seq DESC LIMIT 1";
    let content: String = connection.query_row(sql, [id.0 as i64], |row| row.get(0)).unwrap();
    assert_eq!(content, r#"[["s",["count"],10]]"#);
    let base = DocumentContent::Base { version: 1, value: json(r#"{"count":11}"#) };
    storage.commit(&[change(id, base)], ctx()).await.unwrap();
    assert_eq!(revision_count(&path, id), 1);
    storage.commit(&[change(id, delta(r#"[["r",{"count":12}]]"#))], ctx()).await.unwrap();
    assert_eq!(revision_count(&path, id), 2);
    storage.commit(&[StorageWrite::DocumentRetire { id }], ctx()).await.unwrap();
    assert_eq!(revision_count(&path, id), 0);
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn auto_checkpoints_wal_frames_and_truncates_the_wal_on_close() {
    let (_directory, path) = temp_path();
    let options = SqliteStorageOptions { wal_autocheckpoint_pages: Some(1), ..Default::default() };
    let storage = open_sqlite_storage_with(&path, options).await.unwrap();
    create_root(&storage).await;
    for index in 0..20 {
        let id = EntryId(storage.mint_id().await.unwrap());
        let data = serde_json::json!({ "text": "x".repeat(32 * 1024), "index": index });
        storage.commit(&[StorageWrite::Entry { value: entry(id, Some(Json::from(data))) }], ctx()).await.unwrap();
    }
    let wal_path = format!("{path}-wal");
    assert!(std::fs::metadata(&wal_path).unwrap().len() < 512 * 1024);
    let observer = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let count: i64 = observer.query_row("SELECT count(*) FROM entries", [], |row| row.get(0)).unwrap();
    assert_eq!(count, 20);
    storage.close(ctx()).await.unwrap();
    assert_eq!(std::fs::metadata(&wal_path).unwrap().len(), 0);
}

#[tokio::test]
async fn reuses_pages_released_by_current_only_checkpoints() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    let id = DocumentId(storage.mint_id().await.unwrap());
    let large = Json::from(serde_json::json!({ "text": "x".repeat(512 * 1024) }));
    storage.commit(&[create(session_document(id, "reuse"), large.clone())], ctx()).await.unwrap();
    let small = DocumentContent::Base { version: 1, value: json(r#"{"text":"small"}"#) };
    storage.commit(&[change(id, small)], ctx()).await.unwrap();
    let pages_after_delete = scalar(&path, "SELECT page_count FROM pragma_page_count()");
    let free_after_delete = scalar(&path, "SELECT freelist_count FROM pragma_freelist_count()");
    assert!(free_after_delete > 0);
    storage.commit(&[change(id, DocumentContent::Base { version: 1, value: large })], ctx()).await.unwrap();
    let pages_after_reuse = scalar(&path, "SELECT page_count FROM pragma_page_count()");
    let free_after_reuse = scalar(&path, "SELECT freelist_count FROM pragma_freelist_count()");
    assert!(pages_after_reuse <= pages_after_delete + 2);
    assert!(free_after_reuse < free_after_delete);
    storage.close(ctx()).await.unwrap();
}

#[tokio::test]
async fn keeps_representative_row_and_document_storage_bounded() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    create_root(&storage).await;
    for index in 0..100 {
        let id = EntryId(storage.mint_id().await.unwrap());
        let data = serde_json::json!({ "index": index, "text": "x".repeat(1_024) });
        storage.commit(&[StorageWrite::Entry { value: entry(id, Some(Json::from(data))) }], ctx()).await.unwrap();
    }
    let id = DocumentId(storage.mint_id().await.unwrap());
    let record = DocumentCreate {
        id,
        kind: "size.history".into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    };
    storage.commit(&[create(record, json(r#"{"count":0}"#))], ctx()).await.unwrap();
    for count in 1..=100 {
        storage.commit(&[change(id, delta(&format!(r#"[["s",["count"],{count}]]"#)))], ctx()).await.unwrap();
    }
    storage.close(ctx()).await.unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < 1024 * 1024);
}
