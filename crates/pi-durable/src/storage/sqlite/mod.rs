//! Portable SQLite implementation of the storage contract (spec section 11.2) over the `SqliteDatabase` facade.
//!
//! One SQL transaction is one commit. Records are stored as their JSON text, with indexed columns beside them.

mod connection;
pub mod database;
pub mod migrations;
mod reads;
mod rows;
pub mod schema;
mod writes;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pi_chord::{CopyJsonOptions, Json, copy_json};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Notify, OnceCell};

pub use connection::{BundledSqliteDatabase, SqliteStorageOptions, open_sqlite_storage, open_sqlite_storage_with};
pub use database::{SqliteDatabase, SqliteExecutor, SqliteRow, SqliteValue, TransactionCallback, transaction};
pub use migrations::{
    CURRENT_SQLITE_SCHEMA_VERSION, SQLITE_MIGRATIONS, SqliteMigration, apply_sqlite_migrations,
    apply_sqlite_migrations_for_schema, apply_sqlite_migrations_to_table, create_sqlite_migrations,
};
pub use schema::{SqliteSchema, validate_sqlite_schema_prefix};

use crate::errors::{DurableError, DurableResult};
use crate::types::DocumentScope;

/// SQLite-backed `Storage` over an owned database facade.
pub struct SqliteStorage {
    db: Arc<dyn SqliteDatabase>,
    schema: SqliteSchema,
    next_id: std::sync::Mutex<u64>,
    closed: AtomicBool,
    closing: OnceCell<DurableResult<()>>,
    admitted_reads: AtomicUsize,
    reads_drained: Notify,
}

pub(crate) fn encode<T: Serialize + ?Sized>(value: &T) -> DurableResult<String> {
    Ok(copy_json(value, CopyJsonOptions { omit_undefined_properties: true })?.to_string())
}

pub(crate) fn decode<T: DeserializeOwned>(text: &str) -> DurableResult<T> {
    let json = Json::parse(text).map_err(|error| DurableError::storage(format!("Corrupt SQLite record: {error}")))?;
    json.deserialize_into().map_err(|error| DurableError::storage(format!("Corrupt SQLite record: {error}")))
}

/// Indexed strings are stored JSON-encoded, as upstream does to keep every identity lossless.
pub(crate) fn indexed(value: &str) -> SqliteValue {
    SqliteValue::Text(Json::from(value).to_string())
}

/// The `scope_kind` and `owner_id` columns of a document scope.
pub(crate) fn scope_columns(scope: &DocumentScope) -> (SqliteValue, SqliteValue) {
    (SqliteValue::from(scope.kind()), SqliteValue::from(scope.owner_id().unwrap_or(0)))
}

/// Decrements the admitted read count when a read ends, however it ends.
struct ReadGuard<'a>(&'a SqliteStorage);

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        if self.0.admitted_reads.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.reads_drained.notify_waiters();
        }
    }
}

impl SqliteStorage {
    /// Initialize storage over an owned SQLite database facade; the facade is closed when this fails.
    pub async fn open(db: Arc<dyn SqliteDatabase>) -> DurableResult<Self> {
        Self::open_with_prefix(db, None).await
    }

    /// Like [`SqliteStorage::open`], with every table and index name prefixed so the database can hold other
    /// tables. Changing the prefix does not migrate existing data.
    pub async fn open_with_prefix(db: Arc<dyn SqliteDatabase>, prefix: Option<&str>) -> DurableResult<Self> {
        let opened = async {
            let schema = SqliteSchema::new(prefix)?;
            apply_sqlite_migrations_for_schema(&*db, &schema, &create_sqlite_migrations(&schema)).await?;
            let sql = format!("SELECT next_id, next_seq FROM {} WHERE singleton = 1", schema.tables.durable_metadata);
            let row = db.get(&sql, &[]).await?;
            let row = row.ok_or_else(|| DurableError::storage("Durable SQLite metadata is missing"))?;
            Ok::<_, DurableError>((schema, parse_next_id(row.text("next_id")?)?))
        }
        .await;
        match opened {
            Ok((schema, next_id)) => Ok(Self {
                db,
                schema,
                next_id: std::sync::Mutex::new(next_id),
                closed: AtomicBool::new(false),
                closing: OnceCell::new(),
                admitted_reads: AtomicUsize::new(0),
                reads_drained: Notify::new(),
            }),
            Err(error) => {
                let _ = db.close().await;
                Err(error)
            }
        }
    }

    /// The database facade this storage owns.
    pub fn database(&self) -> &Arc<dyn SqliteDatabase> {
        &self.db
    }

    fn assert_open(&self) -> DurableResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DurableError::storage("SqliteStorage is closed"));
        }
        Ok(())
    }

    /// Admit a read; close waits for admitted reads, so their later queries never reach a closed database.
    fn admit_read(&self) -> DurableResult<ReadGuard<'_>> {
        self.assert_open()?;
        self.admitted_reads.fetch_add(1, Ordering::SeqCst);
        Ok(ReadGuard(self))
    }

    fn next_id(&self) -> u64 {
        *self.next_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn close_database(&self) -> DurableResult<()> {
        loop {
            let drained = self.reads_drained.notified();
            if self.admitted_reads.load(Ordering::SeqCst) == 0 {
                break;
            }
            drained.await;
        }
        self.db.close().await
    }
}

pub(crate) fn parse_next_id(text: &str) -> DurableResult<u64> {
    let value: f64 = text.parse().map_err(|_| DurableError::storage("Durable SQLite metadata is corrupt"))?;
    Ok(if value.is_finite() && value >= 0.0 { value as u64 } else { u64::MAX })
}
