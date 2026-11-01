//! The minimal asynchronous database facade `SqliteStorage` needs, so any SQLite binding can plug in.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::BoxFuture;

use crate::errors::{DurableError, DurableResult};

/// Values supported by the portable SQLite storage core.
#[derive(Clone, Debug, PartialEq)]
pub enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<i64> for SqliteValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<u64> for SqliteValue {
    fn from(value: u64) -> Self {
        Self::Integer(value as i64)
    }
}

impl From<u32> for SqliteValue {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<bool> for SqliteValue {
    fn from(value: bool) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<&str> for SqliteValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<String> for SqliteValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl<T: Into<SqliteValue>> From<Option<T>> for SqliteValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// One result row: column names and their values, in select order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SqliteRow {
    pub columns: Vec<(String, SqliteValue)>,
}

impl SqliteRow {
    pub fn get(&self, column: &str) -> Option<&SqliteValue> {
        self.columns.iter().find(|(name, _)| name == column).map(|(_, value)| value)
    }

    /// An integer column; a missing column or another type is corrupt data.
    pub fn integer(&self, column: &str) -> DurableResult<i64> {
        match self.get(column) {
            Some(SqliteValue::Integer(value)) => Ok(*value),
            _ => Err(DurableError::storage(format!("SQLite column {column} is not an integer"))),
        }
    }

    /// A text column; a missing column or another type is corrupt data.
    pub fn text(&self, column: &str) -> DurableResult<&str> {
        match self.get(column) {
            Some(SqliteValue::Text(value)) => Ok(value),
            _ => Err(DurableError::storage(format!("SQLite column {column} is not text"))),
        }
    }
}

/// Asynchronous SQL operations shared by a database and its transaction handles. `exec` runs SQL text without
/// bindings and may hold several statements; `run`, `get`, and `all` run one statement with positional bindings.
/// Adapters may cache prepared statements by SQL text, so callers bind values instead of interpolating them.
#[async_trait]
pub trait SqliteExecutor: Send + Sync {
    async fn exec(&self, sql: &str) -> DurableResult<()>;
    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()>;
    /// `run`, returning the rows changed as SQLite `changes()` counts them, without index or trigger writes. It may
    /// cost an extra statement, so callers that need no count use `run`.
    async fn run_counted(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<u64>;
    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>>;
    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>>;
}

/// The body of a transaction. It gets the transaction handle and must do all of its work through it.
pub type TransactionCallback<'a> =
    Box<dyn FnOnce(Arc<dyn SqliteExecutor>) -> BoxFuture<'a, DurableResult<()>> + Send + 'a>;

/// Database facade of `SqliteStorage`. A transaction queues other operations until it ends and its handle is
/// invalid after that. A failed callback is rolled back before its error returns; a failed rollback returns a
/// different error, so callers cannot take the callback error for a guaranteed rollback.
#[async_trait]
pub trait SqliteDatabase: SqliteExecutor {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()>;
    async fn close(&self) -> DurableResult<()>;
}

/// Run `callback` in a transaction and return its value.
pub async fn transaction<'a, T, F>(database: &'a dyn SqliteDatabase, callback: F) -> DurableResult<T>
where
    T: Send + 'a,
    F: FnOnce(Arc<dyn SqliteExecutor>) -> BoxFuture<'a, DurableResult<T>> + Send + 'a,
{
    let slot = Arc::new(std::sync::Mutex::new(None));
    let output = slot.clone();
    database
        .transaction(Box::new(move |executor| {
            Box::pin(async move {
                let value = callback(executor).await?;
                *output.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(value);
                Ok(())
            })
        }))
        .await?;
    let value = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    value.ok_or_else(|| DurableError::storage("SQLite transaction returned no value"))
}
