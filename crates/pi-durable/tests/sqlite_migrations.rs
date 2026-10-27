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
async fn migrates_a_version_1_database_to_store_sleeping_tasks_keeping_its_tasks_and_indexes() {
    let (_directory, path) = temp_path();
    let database = open(&path).await;
    apply_sqlite_migrations(&database, &SQLITE_MIGRATIONS[..1]).await.unwrap();
    let waiting = r#"{"id":5,"conversationId":1,"kind":"test.task","version":1,"input":null,"background":false,"abortRequested":false,"state":{"status":"waiting","checkpoint":{"phase":"run"},"on":[],"policy":"allSettled"}}"#;
    let insert = "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record)
        VALUES (?, 1, 'test.task', ?, 0, 0, ?)";
    let row = |id: i64, status: &str, record: &str| {
        [SqliteValue::Integer(id), SqliteValue::Text(status.into()), SqliteValue::Text(record.into())]
    };
    database.run(insert, &row(5, "waiting", waiting)).await.unwrap();
    assert!(database.run(insert, &row(6, "sleeping", "{}")).await.is_err());
    database.close().await.unwrap();

    open_sqlite_storage(&path).await.unwrap().close(&BACKGROUND_CONTEXT).await.unwrap();
    let migrated = open(&path).await;
    assert_eq!(integer(&migrated, VERSION, "version").await, 2);
    assert_eq!(text(&migrated, "SELECT status FROM tasks WHERE id = 5", "status").await, "waiting");
    assert_eq!(text(&migrated, "SELECT record FROM tasks WHERE id = 5", "record").await, waiting);
    migrated.run(insert, &row(6, "sleeping", "{}")).await.unwrap();
    let indexes = migrated
        .all("SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = 'tasks' ORDER BY name", &[])
        .await
        .unwrap();
    let names: Vec<String> = indexes.iter().map(|row| row.text("name").unwrap().to_string()).collect();
    assert_eq!(
        names,
        [
            "tasks_by_abort_requested",
            "tasks_by_background",
            "tasks_by_conversation",
            "tasks_by_kind",
            "tasks_by_status"
        ]
    );
    let leftover = "SELECT count(*) AS count FROM sqlite_schema WHERE name = 'tasks_v2'";
    assert_eq!(integer(&migrated, leftover, "count").await, 0);
    migrated.close().await.unwrap();
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
    storage.commit(&writes, &|_| {}, &BACKGROUND_CONTEXT).await.unwrap();
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
