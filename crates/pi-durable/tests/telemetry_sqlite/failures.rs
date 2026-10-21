//! Port of the write failure and reopen cases of upstream `describe("SQLite telemetry buffering and aggregation")`.

use std::time::Duration;

use pi_durable::errors::DurableError;
use pi_durable::telemetry_sqlite::{SqliteTelemetryOptions, create_sqlite_telemetry};
use pi_telemetry::{
    InstrumentOptions, SpanAttributes, SpanOptions, TelemetryContextExt, TelemetryMetrics, TelemetryPropagation,
};
use serde_json::json;

use crate::support::{Fixture, Spy, clock};

fn add(f: &Fixture, name: &str, value: f64) {
    f.telemetry.counter(name, InstrumentOptions::default()).add(value, SpanAttributes::new());
}

#[tokio::test(start_paused = true)]
async fn stops_permanently_after_a_write_failure_and_reports_the_identical_error_without_retry() {
    let f = Fixture::new(Default::default()).await;
    f.telemetry.start_infallible_span(SpanOptions::new("queued"), |_| async {}).await;
    f.db.fail_with(DurableError::storage("write failure"));
    let error = f.telemetry.flush().await.unwrap_err();
    assert!(matches!(&error, DurableError::Storage(message) if message == "write failure"), "{error:?}");
    assert_eq!(f.telemetry.start_infallible_span(SpanOptions::new("ignored"), |_| async { 1 }).await, 1);
    add(&f, "ignored", 1.0);
    assert_eq!(f.telemetry.flush().await.unwrap_err().to_string(), error.to_string());
    let close = f.telemetry.close();
    assert!(close.ptr_eq(&f.telemetry.close()));
    assert_eq!(close.await.unwrap_err().to_string(), error.to_string());
    assert_eq!(f.db.count(), 1);
    assert_eq!(f.db.rows("SELECT * FROM telemetry_spans").await, json!([]));
}

#[tokio::test(start_paused = true)]
async fn remembers_a_background_write_failure_and_stops_its_timer() {
    let f = Fixture::new(Default::default()).await;
    f.db.fail_with(DurableError::storage("background failure"));
    tokio::time::sleep(Duration::from_millis(1001)).await;
    assert_eq!(f.telemetry.flush().await.unwrap_err().to_string(), "background failure");
    assert_eq!(f.telemetry.close().await.unwrap_err().to_string(), "background failure");
    tokio::time::sleep(Duration::from_millis(5000)).await;
    assert_eq!(f.db.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn rolls_back_spans_and_metric_aggregates_together_on_sql_failure() {
    let f = Fixture::new(Default::default()).await;
    f.telemetry.start_infallible_span(SpanOptions::new("rollback"), |_| async {}).await;
    add(&f, "rollback", 1.0);
    f.db.exec("DROP TABLE telemetry_metrics").await;
    assert!(f.telemetry.close().await.is_err());
    assert_eq!(f.db.rows("SELECT * FROM telemetry_spans").await, json!([]));
}

#[tokio::test(start_paused = true)]
async fn reuses_migrations_preserves_identities_across_reopen_and_merges_aggregates_from_multiple_adapters() {
    let db = Spy::open().await;
    let options = || SqliteTelemetryOptions { clock: Some(clock(20_001.0).0), ..Default::default() };
    let first = create_sqlite_telemetry(db.clone(), options()).await.unwrap();
    let injector = first.clone();
    let carrier = first
        .start_infallible_span(SpanOptions::new("parent"), |span| async move { injector.inject(span.as_ref()) })
        .await;
    first.counter("shared", InstrumentOptions::default()).add(2.0, SpanAttributes::new());
    first.close().await.unwrap();
    let second = create_sqlite_telemetry(db.clone(), options()).await.unwrap();
    let parent = second.extract(carrier.as_ref());
    parent.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
    second.counter("shared", InstrumentOptions::default()).add(4.0, SpanAttributes::new());
    second.close().await.unwrap();

    let rows = db.rows("SELECT * FROM telemetry_spans ORDER BY rowid").await;
    assert_eq!(rows[1]["parent_id"], rows[0]["id"]);
    assert_eq!(rows[1]["trace_id"], rows[0]["trace_id"]);
    assert_eq!(rows[0]["trace_id"], rows[0]["id"]);
    assert_ne!(rows[1]["id"], rows[0]["id"]);
    let metrics = db.rows("SELECT count, sum, min, max FROM telemetry_metrics").await;
    assert_eq!(metrics, json!([{ "count": 2, "sum": 6, "min": 2, "max": 4 }]));
    assert_eq!(db.rows("SELECT version FROM telemetry_schema").await, json!([{ "version": 1 }]));
}
