//! A SQLite connection whose write-ahead log (WAL) a replication engine reads from outside SQLite (upstream
//! `sqlite/replication/node.ts`). It is internal: upstream does not export it from the package yet.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use futures::future::BoxFuture;
use tokio::sync::OnceCell;

use super::connection::{BundledSqliteDatabase, SqliteStorageOptions};
use super::database::{SqliteDatabase, SqliteExecutor, SqliteRow, SqliteValue, TransactionCallback};
use crate::errors::{DurableError, DurableResult};

/// What the replication engine sees on a replicated connection. See [`ReplicatedSqliteDatabase`].
#[async_trait]
pub trait ReplicationHooks: Send + Sync {
    /// Called synchronously after any operation through the connection settles, whether it succeeded or not,
    /// because it may have committed. It must not fail and must not call the database.
    fn after_operation(&self);

    /// Runs during close, alone on the connection, after every earlier operation and before SQLite checkpoints
    /// and deletes the WAL. Everything the engine has not taken out of the WAL by the time this settles is lost.
    async fn before_close(&self, executor: Arc<dyn SqliteExecutor>) -> DurableResult<()>;
}

/// Connection settings for a replicated database file. Automatic WAL checkpoints are always off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReplicatedSqliteOptions {
    /// Time SQLite waits for a competing file lock; see [`SqliteStorageOptions::busy_timeout_ms`].
    pub busy_timeout_ms: Option<u64>,
}

/// A connection whose WAL the engine reads, so only the engine decides when pages leave it. Automatic
/// checkpoints are off, every operation is reported to the hooks, and `close` runs `before_close` before SQLite
/// checkpoints. Only [`open_replicated_sqlite_database`] creates one; it cannot stop another process checkpointing.
pub struct ReplicatedSqliteDatabase {
    path: PathBuf,
    database: BundledSqliteDatabase,
    hooks: Arc<dyn ReplicationHooks>,
    closing: AtomicBool,
    closed: OnceCell<DurableResult<()>>,
}

fn closed_error() -> DurableError {
    DurableError::storage("Replicated SQLite database is closed")
}

impl ReplicatedSqliteDatabase {
    /// Absolute path of the database file. The WAL is `{path}-wal`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn check_open(&self) -> DurableResult<()> {
        if self.closing.load(Ordering::SeqCst) { Err(closed_error()) } else { Ok(()) }
    }

    /// Run `operation` alone on the connection; see [`BundledSqliteDatabase::exclusive`]. It is not reported to
    /// [`ReplicationHooks::after_operation`].
    pub async fn exclusive<'a, T, F>(&'a self, operation: F) -> DurableResult<T>
    where
        T: Send + 'a,
        F: FnOnce(Arc<dyn SqliteExecutor>) -> BoxFuture<'a, DurableResult<T>> + Send + 'a,
    {
        self.check_open()?;
        self.database.exclusive(operation).await
    }

    async fn close_once(&self) -> DurableResult<()> {
        let hook = self.database.exclusive(|executor| self.hooks.before_close(executor)).await;
        let closed = self.database.close().await;
        match (hook, closed) {
            (Err(hook), Err(close)) => {
                Err(DurableError::storage(format!("Replication close hook and close failed: {hook}; {close}")))
            }
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Settle an operation, then tell the engine it ran. A failed operation is reported too.
    async fn reported<T>(&self, operation: impl Future<Output = DurableResult<T>>) -> DurableResult<T> {
        self.check_open()?;
        let result = operation.await;
        self.hooks.after_operation();
        result
    }
}

#[async_trait]
impl SqliteExecutor for ReplicatedSqliteDatabase {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        self.reported(self.database.exec(sql)).await
    }

    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        self.reported(self.database.run(sql, params)).await
    }

    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        self.reported(self.database.get(sql, params)).await
    }

    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        self.reported(self.database.all(sql, params)).await
    }
}

#[async_trait]
impl SqliteDatabase for ReplicatedSqliteDatabase {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()> {
        self.reported(self.database.transaction(callback)).await
    }

    /// Runs the close hook, then closes the connection. Later calls return the first call's result.
    async fn close(&self) -> DurableResult<()> {
        self.closing.store(true, Ordering::SeqCst);
        self.closed.get_or_init(|| self.close_once()).await.clone()
    }
}

/// Open a database file for replication. The file needs a WAL, so in-memory and temporary databases are refused.
pub async fn open_replicated_sqlite_database(
    path: &str,
    hooks: Arc<dyn ReplicationHooks>,
    options: ReplicatedSqliteOptions,
) -> DurableResult<ReplicatedSqliteDatabase> {
    if path.is_empty() || path == ":memory:" || path.starts_with("file:") {
        return Err(DurableError::storage(format!("Replication needs a database file path, got {path:?}")));
    }
    let absolute = std::path::absolute(path).map_err(|error| DurableError::storage(error.to_string()))?;
    let storage_options = SqliteStorageOptions {
        wal_autocheckpoint_pages: Some(0),
        busy_timeout_ms: options.busy_timeout_ms,
        prefix: None,
    };
    let database = BundledSqliteDatabase::open(&absolute.to_string_lossy(), storage_options).await?;
    Ok(ReplicatedSqliteDatabase {
        path: absolute,
        database,
        hooks,
        closing: AtomicBool::new(false),
        closed: OnceCell::new(),
    })
}
