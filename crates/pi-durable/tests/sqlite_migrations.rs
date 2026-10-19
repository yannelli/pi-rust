//! Port of upstream `test/sqlite-migrations.test.ts`.

use pi_chord::{BACKGROUND_CONTEXT, Json};
use pi_durable::storage::sqlite::{
    BundledSqliteDatabase, CURRENT_SQLITE_SCHEMA_VERSION, SQLITE_MIGRATIONS, SqliteDatabase, SqliteExecutor,
    SqliteMigration, SqliteStorageOptions, SqliteValue, apply_sqlite_migrations, open_sqlite_storage,
};
use pi_durable::*;

fn temp_path() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite").to_string_lossy().into_owned();
    (directory, path)
}

async fn open(path: &str) -> BundledSqliteDatabase {
    BundledSqliteDatabase::open(path, SqliteStorageOptions::default()).await.unwrap()
}

async fn integer(database: &BundledSqliteDatabase, sql: &str, column: &str) -> i64 {
    database.get(sql, &[]).await.unwrap().unwrap().integer(column).unwrap()
}

async fn text(database: &BundledSqliteDatabase, sql: &str, column: &str) -> String {
    database.get(sql, &[]).await.unwrap().unwrap().text(column).unwrap().to_string()
}

const VERSION: &str = "SELECT version FROM durable_schema WHERE singleton = 1";
const METADATA: &str = "SELECT next_id, next_seq FROM durable_metadata WHERE singleton = 1";

#[tokio::test]
async fn creates_the_current_schema_and_can_be_applied_repeatedly() {
    let (_directory, path) = temp_path();
    let database = open(&path).await;
    apply_sqlite_migrations(&database, &SQLITE_MIGRATIONS).await.unwrap();
    apply_sqlite_migrations(&database, &SQLITE_MIGRATIONS).await.unwrap();
    assert_eq!(integer(&database, VERSION, "version").await, CURRENT_SQLITE_SCHEMA_VERSION);
    assert_eq!(text(&database, METADATA, "next_id").await, "2");
    assert_eq!(integer(&database, METADATA, "next_seq").await, 1);
    database.close().await.unwrap();
}

#[tokio::test]
async fn rejects_a_database_newer_than_the_portable_core() {
    let (_directory, path) = temp_path();
    let database = open(&path).await;
    apply_sqlite_migrations(&database, &SQLITE_MIGRATIONS).await.unwrap();
    let newer = SqliteValue::Integer(CURRENT_SQLITE_SCHEMA_VERSION + 1);
    database.run("UPDATE durable_schema SET version = ? WHERE singleton = 1", &[newer]).await.unwrap();
    database.close().await.unwrap();
    let error = open_sqlite_storage(&path).await.err().expect("a newer schema");
    assert!(error.to_string().contains("is newer than supported version"), "{error}");
}

#[tokio::test]
async fn rolls_bootstrap_and_every_pending_migration_back_together() {
    let (_directory, path) = temp_path();
    let database = open(&path).await;
    let first = SqliteMigration {
        version: 1,
        statements: vec![
            "CREATE TABLE migration_first (value TEXT) STRICT".into(),
            "INSERT INTO migration_first (value) VALUES ('retained')".into(),
        ],
    };
    let failed = [
        first.clone(),
        SqliteMigration {
            version: 2,
            statements: vec!["CREATE TABLE migration_second (value TEXT) STRICT".into(), "THIS IS NOT SQL".into()],
        },
    ];
    assert!(apply_sqlite_migrations(&database, &failed).await.is_err());
    let count = "SELECT count(*) AS count FROM sqlite_schema
        WHERE name IN ('durable_schema', 'migration_first', 'migration_second')";
    assert_eq!(integer(&database, count, "count").await, 0);

    let fixed = [
        first,
        SqliteMigration { version: 2, statements: vec!["CREATE TABLE migration_second (value TEXT) STRICT".into()] },
    ];
    apply_sqlite_migrations(&database, &fixed).await.unwrap();
    assert_eq!(integer(&database, VERSION, "version").await, 2);
    assert_eq!(text(&database, "SELECT value FROM migration_first", "value").await, "retained");
    database.close().await.unwrap();
}

#[tokio::test]
async fn rolls_a_failed_migration_back_and_keeps_stored_data_for_a_retry() {
    let (_directory, path) = temp_path();
    let storage = open_sqlite_storage(&path).await.unwrap();
    let entry = EntryRecord {
        kind: "retained".into(),
        model: None,
        data: Some(Json::parse(r#"{"retained":true}"#).unwrap()),
        edits: None,
        id: EntryId(2),
        conversation_id: ROOT_CONVERSATION_ID,
        head: None,
        by_task_id: None,
    };
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    let writes = [StorageWrite::Conversation { value: root }, StorageWrite::Entry { value: entry }];
    storage.commit(&writes, &BACKGROUND_CONTEXT).await.unwrap();
    storage.close(&BACKGROUND_CONTEXT).await.unwrap();

    let database = open(&path).await;
    let next_version = CURRENT_SQLITE_SCHEMA_VERSION + 1;
    let mut failed = SQLITE_MIGRATIONS.to_vec();
    failed.push(SqliteMigration {
        version: next_version,
        statements: vec!["CREATE TABLE migration_probe (value TEXT) STRICT".into(), "THIS IS NOT SQL".into()],
    });
    assert!(apply_sqlite_migrations(&database, &failed).await.is_err());
    assert_eq!(integer(&database, VERSION, "version").await, CURRENT_SQLITE_SCHEMA_VERSION);
    let probe = "SELECT count(*) AS count FROM sqlite_schema WHERE type = 'table' AND name = 'migration_probe'";
    assert_eq!(integer(&database, probe, "count").await, 0);

    let mut successful = SQLITE_MIGRATIONS.to_vec();
    successful.push(SqliteMigration {
        version: next_version,
        statements: vec!["CREATE TABLE migration_probe (value TEXT) STRICT".into()],
    });
    apply_sqlite_migrations(&database, &successful).await.unwrap();
    assert_eq!(integer(&database, VERSION, "version").await, next_version);
    let row = database.get("SELECT record, commit_seq FROM entries WHERE id = 2", &[]).await.unwrap().unwrap();
    // Rust writes the entry in its struct order (the order the Session builds it), not the test literal's order.
    assert_eq!(
        row.text("record").unwrap(),
        r#"{"kind":"retained","data":{"retained":true},"id":2,"conversationId":1}"#
    );
    assert_eq!(row.integer("commit_seq").unwrap(), 1);
    assert_eq!(text(&database, METADATA, "next_id").await, "3");
    assert_eq!(integer(&database, METADATA, "next_seq").await, 2);
    database.close().await.unwrap();
}
