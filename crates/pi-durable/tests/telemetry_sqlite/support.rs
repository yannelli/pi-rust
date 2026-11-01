//! Fixtures of the SQLite telemetry tests: a database that counts, gates, or fails transactions as upstream's
//! `vi.spyOn(db, "transaction")` does, and a settable wall clock in place of a mocked `Date.now`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use pi_durable::DurableResult;
use pi_durable::errors::DurableError;
use pi_durable::storage::sqlite::{
    BundledSqliteDatabase, SqliteDatabase, SqliteExecutor, SqliteRow, SqliteStorageOptions, SqliteValue,
    TransactionCallback,
};
use pi_durable::telemetry_sqlite::{SqliteTelemetry, SqliteTelemetryOptions, create_sqlite_telemetry};
use pi_telemetry::TelemetryClock;
use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

pub type Time = Arc<Mutex<f64>>;

pub fn set(time: &Time, value: f64) {
    *time.lock().unwrap() = value;
}

/// A clock whose wall time is `start` until changed; durations come from the real monotonic clock.
pub fn clock(start: f64) -> (TelemetryClock, Time) {
    let now: Time = Arc::new(Mutex::new(start));
    let (wall, origin) = (now.clone(), Instant::now());
    let clock = TelemetryClock::new(move || *wall.lock().unwrap(), move || origin.elapsed().as_secs_f64() * 1000.0);
    (clock, now)
}

struct Gate {
    entered: Arc<Notify>,
    released: watch::Receiver<bool>,
}

/// Delegates to an in-memory database. Transactions can be counted, held at a gate, or failed.
pub struct Spy {
    inner: BundledSqliteDatabase,
    pub transactions: AtomicUsize,
    /// `all` calls made through transaction handles.
    pub queries: Arc<AtomicUsize>,
    pub active: AtomicUsize,
    pub maximum: AtomicUsize,
    gate: Mutex<Option<Gate>>,
    failure: Mutex<Option<DurableError>>,
}

impl Spy {
    pub async fn open() -> Arc<Self> {
        let inner = BundledSqliteDatabase::open(":memory:", SqliteStorageOptions::default()).await.unwrap();
        Arc::new(Self {
            inner,
            transactions: AtomicUsize::new(0),
            queries: Arc::new(AtomicUsize::new(0)),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            gate: Mutex::new(None),
            failure: Mutex::new(None),
        })
    }

    /// Hold every later transaction until the returned sender sends `true`; `entered` is notified on arrival.
    pub fn hold(&self) -> (Arc<Notify>, watch::Sender<bool>) {
        let (release, released) = watch::channel(false);
        let entered = Arc::new(Notify::new());
        *self.gate.lock().unwrap() = Some(Gate { entered: entered.clone(), released });
        (entered, release)
    }

    /// Reject every later transaction with `error` without running it.
    pub fn fail_with(&self, error: DurableError) {
        *self.failure.lock().unwrap() = Some(error);
    }

    pub fn count(&self) -> usize {
        self.transactions.load(Ordering::SeqCst)
    }

    /// Rows as JSON objects; integral reals become integers, as JavaScript numbers do.
    pub async fn rows(&self, sql: &str) -> Value {
        let rows = self.inner.all(sql, &[]).await.unwrap();
        Value::Array(rows.iter().map(row_json).collect())
    }

    pub async fn run(&self, sql: &str, params: &[SqliteValue]) {
        self.inner.run(sql, params).await.unwrap();
    }

    pub async fn exec(&self, sql: &str) {
        self.inner.exec(sql).await.unwrap();
    }
}

pub fn row_json(row: &SqliteRow) -> Value {
    let mut object = Map::new();
    for (name, value) in &row.columns {
        let value = match value {
            SqliteValue::Null => Value::Null,
            SqliteValue::Integer(value) => json!(value),
            SqliteValue::Real(value) if value.fract() == 0.0 && value.abs() < 9e15 => json!(*value as i64),
            SqliteValue::Real(value) => json!(value),
            SqliteValue::Text(value) => json!(value),
            SqliteValue::Blob(value) => json!(value),
        };
        object.insert(name.clone(), value);
    }
    Value::Object(object)
}

struct Counted {
    inner: Arc<dyn SqliteExecutor>,
    queries: Arc<AtomicUsize>,
}

#[async_trait]
impl SqliteExecutor for Counted {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        self.inner.exec(sql).await
    }

    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        self.inner.run(sql, params).await
    }

    async fn run_counted(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<u64> {
        self.inner.run_counted(sql, params).await
    }

    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        self.inner.get(sql, params).await
    }

    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        self.inner.all(sql, params).await
    }
}

#[async_trait]
impl SqliteExecutor for Spy {
    async fn exec(&self, sql: &str) -> DurableResult<()> {
        self.inner.exec(sql).await
    }

    async fn run(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<()> {
        self.inner.run(sql, params).await
    }

    async fn run_counted(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<u64> {
        self.inner.run_counted(sql, params).await
    }

    async fn get(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Option<SqliteRow>> {
        self.inner.get(sql, params).await
    }

    async fn all(&self, sql: &str, params: &[SqliteValue]) -> DurableResult<Vec<SqliteRow>> {
        self.inner.all(sql, params).await
    }
}

#[async_trait]
impl SqliteDatabase for Spy {
    async fn transaction<'a>(&'a self, callback: TransactionCallback<'a>) -> DurableResult<()> {
        self.transactions.fetch_add(1, Ordering::SeqCst);
        if let Some(error) = self.failure.lock().unwrap().clone() {
            return Err(error);
        }
        let gate = self.gate.lock().unwrap().as_ref().map(|gate| (gate.entered.clone(), gate.released.clone()));
        if let Some((entered, mut released)) = gate {
            entered.notify_one();
            released.wait_for(|released| *released).await.unwrap();
        }
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        let queries = self.queries.clone();
        let counted: TransactionCallback<'a> =
            Box::new(move |inner| callback(Arc::new(Counted { inner, queries }) as Arc<dyn SqliteExecutor>));
        let result = self.inner.transaction(counted).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }

    async fn close(&self) -> DurableResult<()> {
        self.inner.close().await
    }
}

pub struct Fixture {
    pub db: Arc<Spy>,
    pub telemetry: SqliteTelemetry,
    pub now: Option<Time>,
}

impl Fixture {
    pub async fn new(options: SqliteTelemetryOptions) -> Self {
        let db = Spy::open().await;
        let telemetry = create_sqlite_telemetry(db.clone(), options).await.unwrap();
        db.transactions.store(0, Ordering::SeqCst);
        Self { db, telemetry, now: None }
    }

    /// A fixture whose wall clock starts at `start`.
    pub async fn at(start: f64, options: SqliteTelemetryOptions) -> Self {
        let (clock, now) = clock(start);
        let mut fixture = Self::new(SqliteTelemetryOptions { clock: Some(clock), ..options }).await;
        fixture.now = Some(now);
        fixture
    }

    pub fn set_now(&self, value: f64) {
        set(self.now.as_ref().expect("fixture has a fixed clock"), value);
    }

    pub async fn flush(&self) {
        self.telemetry.flush().await.unwrap();
    }
}
