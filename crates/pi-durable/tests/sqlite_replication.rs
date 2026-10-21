//! Port of upstream `chord/test/sqlite/replication-node.test.ts`. The upstream case "can only be created by
//! opening a file" has no counterpart: the struct has no public constructor.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_durable::storage::sqlite::replication::{
    ReplicatedSqliteDatabase, ReplicatedSqliteOptions, ReplicationHooks, open_replicated_sqlite_database,
};
use pi_durable::storage::sqlite::{SqliteDatabase, SqliteExecutor, SqliteRow, SqliteValue};
use pi_durable::{DurableError, DurableResult};
use tokio::task::yield_now;

const WAL_HEADER_BYTES: usize = 32;
const WAL_FRAME_HEADER_BYTES: usize = 24;

/// Committed frames in a WAL file, counted from its header without validating checksums.
fn wal_commit_frames(path: &str) -> usize {
    let wal = std::fs::read(path).unwrap_or_default();
    if wal.len() < WAL_HEADER_BYTES {
        return 0;
    }
    let big_endian = |offset: usize| u32::from_be_bytes(wal[offset..offset + 4].try_into().unwrap());
    let page_size = big_endian(8) as usize;
    let (mut commits, mut offset) = (0, WAL_HEADER_BYTES);
    while offset + WAL_FRAME_HEADER_BYTES + page_size <= wal.len() {
        if big_endian(offset + 4) != 0 {
            commits += 1;
        }
        offset += WAL_FRAME_HEADER_BYTES + page_size;
    }
    commits
}

type CloseHook = Box<dyn Fn(Arc<dyn SqliteExecutor>) -> BoxFuture<'static, DurableResult<()>> + Send + Sync>;

struct Recording {
    operations: AtomicUsize,
    before_close: CloseHook,
}

#[async_trait]
impl ReplicationHooks for Recording {
    fn after_operation(&self) {
        self.operations.fetch_add(1, Ordering::SeqCst);
    }

    async fn before_close(&self, executor: Arc<dyn SqliteExecutor>) -> DurableResult<()> {
        (self.before_close)(executor).await
    }
}

fn recording() -> Arc<Recording> {
    recording_with(Box::new(|_| Box::pin(async { Ok(()) })))
}

fn recording_with(before_close: CloseHook) -> Arc<Recording> {
    Arc::new(Recording { operations: AtomicUsize::new(0), before_close })
}

struct Fixture {
    _directory: tempfile::TempDir,
    path: String,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("replica.sqlite").to_string_lossy().into_owned();
    Fixture { _directory: directory, path }
}

async fn open(path: &str, hooks: &Arc<Recording>) -> ReplicatedSqliteDatabase {
    open_replicated_sqlite_database(path, hooks.clone(), ReplicatedSqliteOptions::default()).await.unwrap()
}

fn insert(sql: &'static str, value: &str) -> (&'static str, [SqliteValue; 1]) {
    (sql, [SqliteValue::from(value)])
}

#[tokio::test]
async fn refuses_databases_without_a_wal_file() {
    for target in ["", ":memory:", "file::memory:"] {
        let result = open_replicated_sqlite_database(target, recording(), ReplicatedSqliteOptions::default()).await;
        let error = result.err().expect("a database without a file is refused");
        assert!(error.to_string().contains("Replication needs a database file path"), "{error}");
    }
}

#[tokio::test]
async fn opens_in_wal_mode_and_never_checkpoints_on_its_own() {
    let files = fixture();
    let database = open(&files.path, &recording()).await;
    assert_eq!(database.path(), std::path::Path::new(&files.path));
    let mode = database.get("PRAGMA journal_mode", &[]).await.unwrap().unwrap();
    assert_eq!(mode.text("journal_mode").unwrap(), "wal");
    let pages = database.get("PRAGMA wal_autocheckpoint", &[]).await.unwrap().unwrap();
    assert_eq!(pages.integer("wal_autocheckpoint").unwrap(), 0);
    database.exec("CREATE TABLE pages (value BLOB)").await.unwrap();
    // SQLite's default would checkpoint once the WAL passes 1,000 pages; write well past that.
    for _ in 0..12 {
        let write: pi_durable::storage::sqlite::TransactionCallback<'_> = Box::new(|transaction| {
            Box::pin(async move {
                for _ in 0..100 {
                    transaction.exec("INSERT INTO pages (value) VALUES (randomblob(4000))").await?;
                }
                Ok(())
            })
        });
        database.transaction(write).await.unwrap();
    }
    let wal = format!("{}-wal", files.path);
    assert_eq!(wal_commit_frames(&wal), 13);
    assert!(std::fs::metadata(&wal).unwrap().len() > 1_200 * 4_096);
    database.close().await.unwrap();
}

#[tokio::test]
async fn reports_every_operation_including_failed_ones_and_statements_outside_transactions() {
    let files = fixture();
    let hooks = recording();
    let database = open(&files.path, &hooks).await;
    database.exec("CREATE TABLE notes (value TEXT NOT NULL)").await.unwrap();
    let (sql, params) = insert("INSERT INTO notes (value) VALUES (?)", "autocommit");
    database.run(sql, &params).await.unwrap();
    database.get("SELECT value FROM notes", &[]).await.unwrap();
    database.all("SELECT value FROM notes", &[]).await.unwrap();
    let write: pi_durable::storage::sqlite::TransactionCallback<'_> = Box::new(|transaction| {
        Box::pin(async move {
            let (sql, params) = insert("INSERT INTO notes (value) VALUES (?)", "transaction");
            transaction.run(sql, &params).await
        })
    });
    database.transaction(write).await.unwrap();
    assert!(database.run("INSERT INTO notes (value) VALUES (NULL)", &[]).await.is_err());
    assert_eq!(hooks.operations.load(Ordering::SeqCst), 6);
    database
        .exclusive(|executor| Box::pin(async move { executor.exec("PRAGMA wal_checkpoint(TRUNCATE)").await }))
        .await
        .unwrap();
    assert_eq!(hooks.operations.load(Ordering::SeqCst), 6);
    database.close().await.unwrap();
}

#[tokio::test]
async fn runs_the_close_hook_after_earlier_operations_while_the_wal_holds_every_commit() {
    let files = fixture();
    let wal = format!("{}-wal", files.path);
    let seen: Arc<Mutex<Option<(usize, SqliteRow)>>> = Arc::default();
    let (slot, wal_path) = (seen.clone(), wal.clone());
    let hooks = recording_with(Box::new(move |executor| {
        let (slot, wal_path) = (slot.clone(), wal_path.clone());
        Box::pin(async move {
            let frames = wal_commit_frames(&wal_path);
            let rows = executor.get("SELECT count(*) AS count FROM notes", &[]).await?;
            *slot.lock().unwrap() = rows.map(|row| (frames, row));
            Ok(())
        })
    }));
    let database = Arc::new(open(&files.path, &hooks).await);
    database.exec("CREATE TABLE notes (value TEXT)").await.unwrap();
    // A held exclusive operation queues the inserts and then the close behind it, in that order.
    let (release, gate) = tokio::sync::oneshot::channel::<()>();
    let holder = database.clone();
    let held = tokio::spawn(async move {
        holder.exclusive(|_| Box::pin(async move { gate.await.map_err(|_| DurableError::error("gate dropped")) })).await
    });
    yield_now().await;
    let pending: Vec<_> = ["1", "2", "3"]
        .into_iter()
        .map(|value| {
            let database = database.clone();
            tokio::spawn(async move {
                let (sql, params) = insert("INSERT INTO notes (value) VALUES (?)", value);
                database.run(sql, &params).await
            })
        })
        .collect();
    yield_now().await;
    let closer = database.clone();
    let closed = tokio::spawn(async move { closer.close().await });
    yield_now().await;
    assert!(!closed.is_finished());
    let (sql, params) = insert("INSERT INTO notes (value) VALUES (?)", "late");
    let late = database.run(sql, &params).await.unwrap_err();
    assert!(late.to_string().contains("Replicated SQLite database is closed"));

    release.send(()).unwrap();
    held.await.unwrap().unwrap();
    for write in pending {
        write.await.unwrap().unwrap();
    }
    closed.await.unwrap().unwrap();
    database.close().await.unwrap();

    let (frames, row) = seen.lock().unwrap().take().expect("the close hook ran");
    assert_eq!(frames, 4);
    assert_eq!(row.integer("count").unwrap(), 3);
    // SQLite checkpoints and removes the WAL when the last connection closes.
    assert!(!std::path::Path::new(&wal).exists());
    let exclusive = database.exclusive(|_| Box::pin(async { Ok(()) })).await.unwrap_err();
    assert!(exclusive.to_string().contains("Replicated SQLite database is closed"));
}

#[tokio::test]
async fn closes_the_connection_even_when_the_close_hook_fails() {
    let files = fixture();
    let hooks = recording_with(Box::new(|_| Box::pin(async { Err(DurableError::error("spool failed")) })));
    let database = open(&files.path, &hooks).await;
    database.exec("CREATE TABLE notes (value TEXT)").await.unwrap();
    let error = database.close().await.unwrap_err();
    assert!(error.to_string().contains("spool failed"));
    assert!(database.close().await.unwrap_err().to_string().contains("spool failed"));
    assert!(!std::path::Path::new(&format!("{}-wal", files.path)).exists());

    let reopened = open(&files.path, &recording()).await;
    assert!(reopened.all("SELECT value FROM notes", &[]).await.unwrap().is_empty());
    reopened.close().await.unwrap();
}
