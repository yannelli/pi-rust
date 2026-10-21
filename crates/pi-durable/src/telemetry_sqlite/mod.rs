//! SQLite telemetry (upstream chord `telemetry-sqlite`): a buffered recorder of settled spans and ten-second metric
//! aggregates, and a [`TelemetryStore`] over the same rows. Tables, SQL, and JSON columns match upstream, so a
//! database written by either side can be read and extended by the other.

mod encode;
mod record;
mod store;

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use pi_telemetry::{
    MetricQuery, StoreError, StoredMetric, StoredTrace, TelemetryClock, TelemetryMetricKind, TelemetryStore, TracePage,
    TraceQuery,
};
use tokio::time::{Instant, MissedTickBehavior};

use crate::errors::{DurableError, DurableResult};
use crate::storage::sqlite::{SqliteDatabase, SqliteMigration, SqliteValue, apply_sqlite_migrations_to_table};

const INTERVAL_MS: f64 = 10_000.0;
const FLUSH_EVERY: Duration = Duration::from_secs(1);
const DEFAULT_RETENTION_MS: f64 = 7.0 * 24.0 * 60.0 * 60.0 * 1000.0;
const DEFAULT_CAPACITY: usize = 10_000;
/// The table that tracks the telemetry schema version, apart from the durable storage history.
pub const TELEMETRY_SCHEMA_TABLE: &str = "telemetry_schema";

const INSERT_SPAN: &str = "INSERT INTO telemetry_spans VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
const UPSERT_METRIC: &str = "INSERT INTO telemetry_metrics VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
\t\t\t\tON CONFLICT(interval_ms, series_key) DO UPDATE SET count = count + excluded.count,
\t\t\t\tsum = sum + excluded.sum, min = min(min, excluded.min), max = max(max, excluded.max)";

/// The telemetry schema history, byte-for-byte the upstream statements.
pub fn sqlite_telemetry_migrations() -> Vec<SqliteMigration> {
    let statements = [
        "CREATE TABLE telemetry_spans (id TEXT PRIMARY KEY, trace_id TEXT NOT NULL, parent_id TEXT, name TEXT, started_ms REAL,
\t\t\tended_ms REAL NOT NULL, attributes TEXT, events TEXT, links TEXT, status TEXT)",
        "CREATE INDEX telemetry_spans_ended_ms ON telemetry_spans(ended_ms)",
        "CREATE INDEX telemetry_spans_trace_started ON telemetry_spans(trace_id, started_ms)",
        "CREATE INDEX telemetry_spans_started_ms ON telemetry_spans(started_ms)",
        "CREATE TABLE telemetry_metrics (interval_ms INTEGER, series_key TEXT, name TEXT, kind TEXT,
\t\t\tunit TEXT, attributes TEXT, count INTEGER, sum REAL, min REAL, max REAL, PRIMARY KEY(interval_ms, series_key))",
    ];
    vec![SqliteMigration { version: 1, statements: statements.map(String::from).to_vec() }]
}

#[derive(Clone, Default)]
pub struct SqliteTelemetryOptions {
    /// Keep rows for seven days by default. Cleanup runs in the flush transaction.
    pub retention_ms: Option<f64>,
    /// Settled spans awaiting commit, including the in-flight batch. Defaults to 10,000.
    /// Also bounds pending metric interval/series pairs independently.
    pub max_queued_spans: Option<usize>,
    /// Time sources for span times and metric intervals; defaults to the system clocks.
    pub clock: Option<TelemetryClock>,
}

/// The shared outcome of [`SqliteTelemetry::close`]; every call returns the same one.
pub type SqliteTelemetryClose = Shared<BoxFuture<'static, DurableResult<()>>>;

type SpanRow = [SqliteValue; 10];

/// One metric series over one interval, aggregated in memory until a flush.
struct Metric {
    interval: i64,
    key: String,
    name: String,
    kind: TelemetryMetricKind,
    unit: Option<String>,
    attributes: String,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Metric {
    fn params(&self) -> [SqliteValue; 10] {
        [
            SqliteValue::Integer(self.interval),
            SqliteValue::Text(self.key.clone()),
            SqliteValue::Text(self.name.clone()),
            SqliteValue::from(encode::kind(self.kind)),
            SqliteValue::from(self.unit.clone()),
            SqliteValue::Text(self.attributes.clone()),
            SqliteValue::from(self.count),
            SqliteValue::Real(self.sum),
            SqliteValue::Real(self.min),
            SqliteValue::Real(self.max),
        ]
    }
}

#[derive(Default)]
struct State {
    sequence: u64,
    spans: Vec<SpanRow>,
    in_flight_spans: usize,
    measurements: Vec<Metric>,
    pending: HashMap<(i64, String), usize>,
    in_flight_measurements: usize,
    stopped: bool,
    writing: bool,
    failure: Option<DurableError>,
    dropped_spans: u64,
    dropped_measurements: u64,
}

struct Recorder {
    db: Arc<dyn SqliteDatabase>,
    retention: f64,
    capacity: usize,
    scope: String,
    clock: TelemetryClock,
    state: Mutex<State>,
    /// Serializes writes, as upstream waits for the write in flight before starting the next.
    writer: tokio::sync::Mutex<()>,
    closing: OnceLock<SqliteTelemetryClose>,
}

fn interval_of(wall_ms: f64) -> i64 {
    ((wall_ms / INTERVAL_MS).floor() * INTERVAL_MS) as i64
}

/// A random UUID like `crypto.randomUUID()`, so span IDs of separate recorders never collide.
fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    crate::util::random_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// Start a write at once and finish it even when the caller stops waiting, as an upstream promise does.
fn spawn(write: impl Future<Output = DurableResult<()>> + Send + 'static) -> BoxFuture<'static, DurableResult<()>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return write.boxed() };
    let task = runtime.spawn(write);
    let joined = task.map(|outcome| {
        outcome.unwrap_or_else(|error| Err(DurableError::storage(format!("Telemetry write stopped: {error}"))))
    });
    joined.boxed()
}

impl Recorder {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_stopped(&self) -> bool {
        self.lock().stopped
    }

    async fn drain(self: Arc<Self>, partial: bool) -> DurableResult<()> {
        let _writer = self.writer.lock().await;
        let completed = interval_of(self.clock.wall_ms());
        let (spans, metrics) = {
            let mut state = self.lock();
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            let spans = std::mem::take(&mut state.spans);
            let (metrics, kept): (Vec<Metric>, Vec<Metric>) =
                std::mem::take(&mut state.measurements).into_iter().partition(|m| partial || m.interval < completed);
            state.pending = kept.iter().enumerate().map(|(index, m)| ((m.interval, m.key.clone()), index)).collect();
            state.measurements = kept;
            state.in_flight_spans = spans.len();
            state.in_flight_measurements = metrics.len();
            state.writing = true;
            (spans, metrics)
        };
        let (clock, retention) = (self.clock.clone(), self.retention);
        let written = self
            .db
            .transaction(Box::new(move |tx| {
                Box::pin(async move {
                    for span in &spans {
                        tx.run(INSERT_SPAN, span).await?;
                    }
                    for metric in &metrics {
                        tx.run(UPSERT_METRIC, &metric.params()).await?;
                    }
                    let cutoff = [SqliteValue::Real(clock.wall_ms() - retention)];
                    tx.run("DELETE FROM telemetry_spans WHERE ended_ms < ?", &cutoff).await?;
                    tx.run("DELETE FROM telemetry_metrics WHERE interval_ms < ?", &cutoff).await
                })
            }))
            .await;
        let mut state = self.lock();
        (state.writing, state.in_flight_spans, state.in_flight_measurements) = (false, 0, 0);
        if let Err(error) = &written {
            state.failure = Some(error.clone());
            state.stopped = true;
            state.spans.clear();
            state.measurements.clear();
            state.pending.clear();
        }
        written
    }
}

/// Flush every second while recording, like upstream's unreferenced interval. It holds no strong reference
/// between ticks and ends once the recorder stops or is dropped.
fn start_timer(recorder: &Arc<Recorder>) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
    let weak = Arc::downgrade(recorder);
    runtime.spawn(async move {
        let mut ticks = tokio::time::interval_at(Instant::now() + FLUSH_EVERY, FLUSH_EVERY);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let Some(recorder) = weak.upgrade() else { return };
            let (stopped, writing) = {
                let state = recorder.lock();
                (state.stopped, state.writing)
            };
            if stopped {
                return;
            }
            if !writing {
                let _ = recorder.drain(false).await;
            }
        }
    });
}

/// Portable, buffered SQLite recorder and store (upstream `SqliteTelemetry`). Recording never runs SQL; a flush
/// runs every second. Only settled spans are stored, and metrics aggregate count, sum, min, and max per series
/// per ten-second interval. A write failure stops recording for good and is reported by `flush` and `close`.
#[derive(Clone)]
pub struct SqliteTelemetry {
    recorder: Arc<Recorder>,
}

/// Apply the telemetry migrations to `db` and start recording into it. The database stays owned by the caller.
pub async fn create_sqlite_telemetry(
    db: Arc<dyn SqliteDatabase>,
    options: SqliteTelemetryOptions,
) -> DurableResult<SqliteTelemetry> {
    apply_sqlite_migrations_to_table(&*db, &sqlite_telemetry_migrations(), TELEMETRY_SCHEMA_TABLE).await?;
    let recorder = Arc::new(Recorder {
        db,
        retention: options.retention_ms.unwrap_or(DEFAULT_RETENTION_MS),
        capacity: options.max_queued_spans.unwrap_or(DEFAULT_CAPACITY),
        scope: random_uuid(),
        clock: options.clock.unwrap_or_default(),
        state: Mutex::new(State::default()),
        writer: tokio::sync::Mutex::new(()),
        closing: OnceLock::new(),
    });
    start_timer(&recorder);
    Ok(SqliteTelemetry { recorder })
}

impl SqliteTelemetry {
    /// The database this recorder writes to and reads from.
    pub fn database(&self) -> &Arc<dyn SqliteDatabase> {
        &self.recorder.db
    }

    /// Settled spans not queued because the queue was full.
    pub fn dropped_spans(&self) -> u64 {
        self.recorder.lock().dropped_spans
    }

    /// Measurements not aggregated because the pending series were at capacity.
    pub fn dropped_measurements(&self) -> u64 {
        self.recorder.lock().dropped_measurements
    }

    /// Persist settled spans and completed ten-second metric intervals. After `close`, this is the close outcome.
    pub fn flush(&self) -> BoxFuture<'static, DurableResult<()>> {
        match self.recorder.closing.get() {
            Some(closing) => closing.clone().boxed(),
            None => spawn(self.recorder.clone().drain(false)),
        }
    }

    /// Stop recording and drain, including partial metric intervals. It does not wait for open spans and does not
    /// close the database.
    pub fn close(&self) -> SqliteTelemetryClose {
        let closing = self.recorder.closing.get_or_init(|| {
            self.recorder.lock().stopped = true;
            spawn(self.recorder.clone().drain(true)).shared()
        });
        closing.clone()
    }
}

/// Readers use committed rows only; flush first to include newly settled spans.
impl TelemetryStore for SqliteTelemetry {
    fn find_traces(&self, query: TraceQuery) -> BoxFuture<'_, Result<TracePage, StoreError>> {
        Box::pin(store::find_traces(&*self.recorder.db, query))
    }

    fn get_trace<'a>(&'a self, trace_id: &'a str) -> BoxFuture<'a, Result<Option<StoredTrace>, StoreError>> {
        Box::pin(store::get_trace(&*self.recorder.db, trace_id))
    }

    fn find_metrics(&self, query: MetricQuery) -> BoxFuture<'_, Result<Vec<StoredMetric>, StoreError>> {
        Box::pin(store::find_metrics(&*self.recorder.db, query))
    }
}
