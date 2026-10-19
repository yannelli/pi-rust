//! `SqliteDatabase` over a bundled SQLite connection (upstream `node.ts`), and the file-backed storage opener.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rusqlite::types::{Value, ValueRef};
use rusqlite::{Connection, params_from_iter};

use super::SqliteStorage;
use super::database::{SqliteDatabase, SqliteExecutor, SqliteRow, SqliteValue, TransactionCallback};
use super::schema::validate_sqlite_schema_prefix;
use crate::errors::{DurableError, DurableResult};

/// Connection settings for a durable storage file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteStorageOptions {
    /// SQLite WAL auto-checkpoint threshold; default 1,000 pages; 0 disables it.
    pub wal_autocheckpoint_pages: Option<u32>,
    /// Time SQLite waits for a competing file lock; default 5,000 ms.
    pub busy_timeout_ms: Option<u64>,
    /// Prefix for every table and index of the storage; see [`SqliteStorage::open_with_prefix`].
    pub prefix: Option<String>,
}

const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: u32 = 1_000;
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 5_000;
/// Large enough that every storage statement stays prepared once per connection.
const STATEMENT_CACHE_CAPACITY: usize = 512;

type SharedConnection = Arc<Mutex<Option<Connection>>>;

fn sqlite_error(error: rusqlite::Error) -> DurableError {
    DurableError::storage(format!("SQLite: {error}"))
}

fn to_sql(value: &SqliteValue) -> Value {
    match value {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(value) => Value::Integer(*value),
        SqliteValue::Real(value) => Value::Real(*value),
        SqliteValue::Text(value) => Value::Text(value.clone()),
        SqliteValue::Blob(value) => Value::Blob(value.clone()),
    }
}

fn from_sql(value: ValueRef<'_>) -> SqliteValue {
    match value {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(value) => SqliteValue::Integer(value),
        ValueRef::Real(value) => SqliteValue::Real(value),
        ValueRef::Text(text) => SqliteValue::Text(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(bytes) => SqliteValue::Blob(bytes.to_vec()),
    }
}

fn with_connection<T>(
    connection: &SharedConnection,
    operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
) -> DurableResult<T> {
    let guard = connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let connection = guard.as_ref().ok_or_else(|| DurableError::storage("SQLite database is closed"))?;
    operation(connection).map_err(sqlite_error)
}

fn query(
    connection: &Connection,
    sql: &str,
    params: &[SqliteValue],
    limit: Option<usize>,
) -> rusqlite::Result<Vec<SqliteRow>> {
    let mut statement = connection.prepare_cached(sql)?;
    let names: Vec<String> = statement.column_names().into_iter().map(String::from).collect();
    let mut rows = statement.query(params_from_iter(params.iter().map(to_sql)))?;
    let mut found = Vec::new();
    while let Some(row) = rows.next()? {
        let mut columns = Vec::with_capacity(names.len());
        for (index, name) in names.iter().enumerate() {
            columns.push((name.clone(), from_sql(row.get_ref(index)?)));
        }
        found.push(SqliteRow { columns });
        if limit.is_some_and(|limit| found.len() >= limit) {
            break;
        }
    }
    Ok(found)
}

fn exec(connection: &SharedConnection, sql: &str) -> DurableResult<()> {
    with_connection(connection, |connection| connection.execute_batch(sql))
}

fn run(connection: &SharedConnection, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
    with_connection(connection, |connection| {
        let mut statement = connection.prepare_cached(sql)?;
        statement.execute(params_from_iter(params.iter().map(to_sql))).map(drop)
    })
}

/// One active transaction's handle; it fails once its transaction has settled.
struct TransactionHandle {
    connection: SharedConnection,
    active: Arc<AtomicBool>,
}

impl TransactionHandle {
    fn check(&self) -> DurableResult<()> {
        if self.active.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(DurableError::storage("SQLite transaction handle is no longer active"))
        }
    }
}

#[async_trait]
impl SqliteExecutor for TransactionHandle {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        self.check()?;
        exec(&self.connection, sql)
    }

    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        self.check()?;
        run(&self.connection, sql, params)
    }

    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        self.check()?;
        with_connection(&self.connection, |connection| query(connection, sql, params, Some(1)))
            .map(|rows| rows.into_iter().next())
    }

    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        self.check()?;
        with_connection(&self.connection, |connection| query(connection, sql, params, None))
    }
}

/// `SqliteDatabase` backed by a bundled SQLite connection. Operations run one at a time in call order; a
/// transaction holds the queue until it commits or rolls back. Statements are cached per connection by SQL text.
pub struct BundledSqliteDatabase {
    connection: SharedConnection,
    access: tokio::sync::Mutex<()>,
}

impl BundledSqliteDatabase {
    pub fn new(connection: Connection) -> Self {
        connection.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
        Self { connection: Arc::new(Mutex::new(Some(connection))), access: tokio::sync::Mutex::new(()) }
    }

    /// Open and configure a database file (or `:memory:`) with WAL and `synchronous = NORMAL`.
    pub async fn open(path: &str, options: SqliteStorageOptions) -> DurableResult<Self> {
        let pages = options.wal_autocheckpoint_pages.unwrap_or(DEFAULT_WAL_AUTOCHECKPOINT_PAGES);
        let timeout = options.busy_timeout_ms.unwrap_or(DEFAULT_BUSY_TIMEOUT_MS);
        if path != ":memory:"
            && let Some(parent) = Path::new(path).parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await.map_err(|error| DurableError::storage(error.to_string()))?;
        }
        let connection = Connection::open(path).map_err(sqlite_error)?;
        connection.busy_timeout(std::time::Duration::from_millis(timeout)).map_err(sqlite_error)?;
        let database = Self::new(connection);
        let configured = async {
            database.exec("PRAGMA journal_mode = WAL").await?;
            database.exec("PRAGMA synchronous = NORMAL").await?;
            database.exec(&format!("PRAGMA wal_autocheckpoint = {pages}")).await
        };
        if let Err(error) = configured.await {
            let _ = database.close().await;
            return Err(error);
        }
        Ok(database)
    }
}

#[async_trait]
impl SqliteExecutor for BundledSqliteDatabase {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        let _access = self.access.lock().await;
        exec(&self.connection, sql)
    }

    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        let _access = self.access.lock().await;
        run(&self.connection, sql, params)
    }

    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        let _access = self.access.lock().await;
        with_connection(&self.connection, |connection| query(connection, sql, params, Some(1)))
            .map(|rows| rows.into_iter().next())
    }

    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        let _access = self.access.lock().await;
        with_connection(&self.connection, |connection| query(connection, sql, params, None))
    }
}

#[async_trait]
impl SqliteDatabase for BundledSqliteDatabase {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()> {
        let _access = self.access.lock().await;
        exec(&self.connection, "BEGIN IMMEDIATE")?;
        let active = Arc::new(AtomicBool::new(true));
        let handle = Arc::new(TransactionHandle { connection: self.connection.clone(), active: active.clone() });
        let result = callback(handle).await;
        active.store(false, Ordering::SeqCst);
        match result {
            Ok(()) => exec(&self.connection, "COMMIT"),
            Err(error) => match exec(&self.connection, "ROLLBACK") {
                Ok(()) => Err(error),
                Err(rollback) => Err(DurableError::storage(format!(
                    "SQLite transaction failed and rollback failed: {error}; {rollback}"
                ))),
            },
        }
    }

    async fn close(&self) -> DurableResult<()> {
        let _access = self.access.lock().await;
        let Some(connection) = self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take() else {
            return Ok(());
        };
        connection.flush_prepared_statement_cache();
        let checkpoint = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").map_err(sqlite_error);
        let closed = connection.close().map_err(|(_, error)| sqlite_error(error));
        checkpoint.and(closed)
    }
}

/// Open or create file-backed durable storage (upstream `openNodeSqliteStorage`).
pub async fn open_sqlite_storage(path: &str) -> DurableResult<SqliteStorage> {
    open_sqlite_storage_with(path, SqliteStorageOptions::default()).await
}

/// Open or create file-backed durable storage with explicit connection settings.
pub async fn open_sqlite_storage_with(path: &str, options: SqliteStorageOptions) -> DurableResult<SqliteStorage> {
    if let Some(prefix) = &options.prefix {
        validate_sqlite_schema_prefix(prefix)?;
    }
    let database = Arc::new(BundledSqliteDatabase::open(path, options.clone()).await?);
    SqliteStorage::open_with_prefix(database, options.prefix.as_deref()).await
}
