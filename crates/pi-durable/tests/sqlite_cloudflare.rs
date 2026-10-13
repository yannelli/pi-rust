//! Port of the facade-level parts of upstream `test/sqlite-cloudflare.test.ts`.
//!
//! Durable Objects do not exist for Rust, so a minimal third-party adapter stands in for the Durable Object one.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_durable::storage::sqlite::{
    SqliteDatabase, SqliteExecutor, SqliteRow, SqliteStorage, SqliteValue, TransactionCallback,
};
use pi_durable::testing::storage_conformance::StorageTest;
use pi_durable::*;
use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};
use tokio::sync::oneshot;

/// Like the Durable Object adapter: plain `BEGIN`, no statement cache, and one serial queue for every operation.
struct PlainDatabase {
    connection: Arc<Mutex<Connection>>,
    queue: tokio::sync::Mutex<()>,
}

struct PlainHandle {
    connection: Arc<Mutex<Connection>>,
    active: Arc<AtomicBool>,
}

fn execute(connection: &Mutex<Connection>, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
    let connection = connection.lock().unwrap();
    let fail = |error: rusqlite::Error| DurableError::storage(error.to_string());
    if params.is_empty() && sql.trim().trim_end_matches(';').contains(';') {
        connection.execute_batch(sql).map_err(fail)?;
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(sql).map_err(fail)?;
    let names: Vec<String> = statement.column_names().into_iter().map(String::from).collect();
    let bound = params.iter().map(|value| match value {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(value) => Value::Integer(*value),
        SqliteValue::Real(value) => Value::Real(*value),
        SqliteValue::Text(value) => Value::Text(value.clone()),
        SqliteValue::Blob(value) => Value::Blob(value.clone()),
    });
    let mut rows = statement.query(rusqlite::params_from_iter(bound)).map_err(fail)?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().map_err(fail)? {
        let mut columns = Vec::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index).map_err(fail)? {
                ValueRef::Null => SqliteValue::Null,
                ValueRef::Integer(value) => SqliteValue::Integer(value),
                ValueRef::Real(value) => SqliteValue::Real(value),
                ValueRef::Text(text) => SqliteValue::Text(String::from_utf8_lossy(text).into_owned()),
                ValueRef::Blob(bytes) => SqliteValue::Blob(bytes.to_vec()),
            };
            columns.push((name.clone(), value));
        }
        found.push(SqliteRow { columns });
    }
    Ok(found)
}

impl PlainHandle {
    fn check(&self) -> DurableResult<()> {
        if self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        Err(DurableError::storage("SQLite transaction handle is no longer active"))
    }
}

#[async_trait]
impl SqliteExecutor for PlainHandle {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        let _guard = self.check()?;
        execute(&self.connection, sql, &[]).map(drop)
    }
    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        let _guard = self.check()?;
        execute(&self.connection, sql, params).map(drop)
    }
    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        let _guard = self.check()?;
        execute(&self.connection, sql, params).map(|rows| rows.into_iter().next())
    }
    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        let _guard = self.check()?;
        execute(&self.connection, sql, params)
    }
}

#[async_trait]
impl SqliteExecutor for PlainDatabase {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        let _guard = self.queue.lock().await;
        execute(&self.connection, sql, &[]).map(drop)
    }
    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        let _guard = self.queue.lock().await;
        execute(&self.connection, sql, params).map(drop)
    }
    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        let _guard = self.queue.lock().await;
        execute(&self.connection, sql, params).map(|rows| rows.into_iter().next())
    }
    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        let _guard = self.queue.lock().await;
        execute(&self.connection, sql, params)
    }
}

#[async_trait]
impl SqliteDatabase for PlainDatabase {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()> {
        let _queue = self.queue.lock().await;
        execute(&self.connection, "BEGIN", &[])?;
        let active = Arc::new(AtomicBool::new(true));
        let result =
            callback(Arc::new(PlainHandle { connection: self.connection.clone(), active: active.clone() })).await;
        active.store(false, Ordering::SeqCst);
        execute(&self.connection, if result.is_ok() { "COMMIT" } else { "ROLLBACK" }, &[])?;
        result
    }

    async fn close(&self) -> DurableResult<()> {
        let _queue = self.queue.lock().await;
        Ok(())
    }
}

fn database() -> Arc<PlainDatabase> {
    let connection = Connection::open_in_memory().unwrap();
    Arc::new(PlainDatabase { connection: Arc::new(Mutex::new(connection)), queue: tokio::sync::Mutex::new(()) })
}

async fn with_plain_storage(test: StorageTest) {
    test(Arc::new(SqliteStorage::open(database()).await.unwrap())).await;
}

mod conformance {
    pi_durable::storage_conformance_tests!(super::with_plain_storage);
}

#[tokio::test]
async fn binds_and_returns_blob_values() {
    let database = database();
    database.exec("CREATE TABLE blobs (id INTEGER PRIMARY KEY, data BLOB, n INTEGER)").await.unwrap();
    let params = [SqliteValue::Integer(1), SqliteValue::Blob(vec![1, 2, 3]), SqliteValue::Integer(7)];
    database.run("INSERT INTO blobs (id, data, n) VALUES (?, ?, ?)", &params).await.unwrap();
    let row = database.get("SELECT data, n FROM blobs WHERE id = ?", &[SqliteValue::Integer(1)]).await.unwrap();
    let row = row.unwrap();
    assert_eq!(row.get("data"), Some(&SqliteValue::Blob(vec![1, 2, 3])));
    assert_eq!(row.get("n"), Some(&SqliteValue::Integer(7)));
}

// "rejects a bigint binding outside the safe integer range" does not apply: Durable Object SQL binds numbers as
// doubles, while `SqliteValue::Integer` is a 64-bit integer that every Rust binding stores exactly.

async fn gated_transaction(
    database: Arc<PlainDatabase>,
    gate: oneshot::Receiver<()>,
    order: Arc<Mutex<Vec<&'static str>>>,
) -> DurableResult<()> {
    database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                handle.run("INSERT INTO items (id) VALUES (?)", &[SqliteValue::Integer(1)]).await?;
                gate.await.unwrap();
                order.lock().unwrap().push("transaction");
                Ok(())
            })
        }))
        .await
}

#[tokio::test]
async fn closes_after_an_active_transaction_settles() {
    let database = database();
    database.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)").await.unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = oneshot::channel();
    let transaction = tokio::spawn(gated_transaction(database.clone(), gate, order.clone()));
    let (closing, closed_order) = (database.clone(), order.clone());
    let closed = tokio::spawn(async move {
        closing.close().await.unwrap();
        closed_order.lock().unwrap().push("close");
    });
    release.send(()).unwrap();
    transaction.await.unwrap().unwrap();
    closed.await.unwrap();
    assert_eq!(*order.lock().unwrap(), ["transaction", "close"]);
}

#[tokio::test]
async fn rolls_back_a_rejected_transaction_and_rejects_its_handle_afterwards() {
    let database = database();
    database.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)").await.unwrap();
    let kept: Arc<Mutex<Option<Arc<dyn SqliteExecutor>>>> = Arc::new(Mutex::new(None));
    let slot = kept.clone();
    let result = database
        .transaction(Box::new(move |handle| {
            Box::pin(async move {
                *slot.lock().unwrap() = Some(handle.clone());
                handle.run("INSERT INTO items (id) VALUES (?)", &[SqliteValue::Integer(1)]).await?;
                Err(DurableError::error("abort"))
            })
        }))
        .await;
    assert!(result.unwrap_err().to_string().contains("abort"));
    assert!(database.all("SELECT id FROM items", &[]).await.unwrap().is_empty());
    let handle = kept.lock().unwrap().take().unwrap();
    let stale = handle.run("INSERT INTO items (id) VALUES (?)", &[SqliteValue::Integer(2)]).await;
    assert!(stale.unwrap_err().to_string().contains("no longer active"));
}

#[tokio::test]
async fn queues_operations_behind_an_active_transaction() {
    let database = database();
    database.exec("CREATE TABLE items (id INTEGER PRIMARY KEY)").await.unwrap();
    let order = Arc::new(Mutex::new(Vec::new()));
    let (release, gate) = oneshot::channel();
    let transaction = tokio::spawn(gated_transaction(database.clone(), gate, order.clone()));
    let (reader, read_order) = (database.clone(), order.clone());
    let read = tokio::spawn(async move {
        let rows = reader.all("SELECT id FROM items", &[]).await.unwrap();
        read_order.lock().unwrap().push("read");
        rows
    });
    tokio::task::yield_now().await;
    release.send(()).unwrap();
    transaction.await.unwrap().unwrap();
    let rows = read.await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("id"), Some(&SqliteValue::Integer(1)));
    assert_eq!(*order.lock().unwrap(), ["transaction", "read"]);
    database.close().await.unwrap();
}
