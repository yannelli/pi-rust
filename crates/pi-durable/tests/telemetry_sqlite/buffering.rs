//! Port of upstream `describe("SQLite telemetry buffering and aggregation")`: queueing, aggregation, capacity,
//! the one-second flush, retention, and close. Failure cases are in `failures.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_durable::telemetry_sqlite::SqliteTelemetryOptions;
use pi_telemetry::{
    HistogramOptions, InstrumentOptions, SpanAttributes, SpanOptions, TelemetryContextExt, TelemetryMetrics,
    TelemetrySpan,
};
use serde_json::{Value, json};

use crate::support::Fixture;

type Slot = Arc<Mutex<Option<Arc<dyn TelemetrySpan>>>>;

/// Every key of `expected` has the same value in `row`, like `toMatchObject`.
pub fn assert_subset(row: &Value, expected: Value) {
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&row[key], value, "{key} of {row}");
    }
}

fn add(f: &Fixture, name: &str, value: f64) {
    f.telemetry.counter(name, InstrumentOptions::default()).add(value, SpanAttributes::new());
}

async fn root(f: &Fixture, name: &str) {
    f.telemetry.start_infallible_span(SpanOptions::new(name), |_| async {}).await;
}

#[tokio::test(start_paused = true)]
async fn does_not_execute_sql_while_recording_and_stores_only_settled_spans() {
    let f = Fixture::new(Default::default()).await;
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let (slot, metrics): (Slot, _) = (Slot::default(), f.telemetry.clone());
    let active = slot.clone();
    let pending = f.telemetry.start_infallible_span(SpanOptions::new("active"), move |span| {
        span.add_event("begin", SpanAttributes::new());
        metrics.counter("calls", InstrumentOptions::default()).add(1.0, SpanAttributes::new());
        *active.lock().unwrap() = Some(span);
        async move {
            let _ = gate.await;
        }
    });
    assert_eq!(f.db.count(), 0);
    f.flush().await;
    assert_eq!(f.db.rows("SELECT * FROM telemetry_spans").await, json!([]));
    release.send(()).unwrap();
    pending.await;
    assert_eq!(f.db.count(), 1);
    let span = slot.lock().unwrap().clone().unwrap();
    span.add_event("late", SpanAttributes::new());
    f.flush().await;
    let row = f.db.rows("SELECT * FROM telemetry_spans").await[0].clone();
    assert!(row["ended_ms"].as_f64().unwrap() >= row["started_ms"].as_f64().unwrap());
    let events: Value = serde_json::from_str(row["events"].as_str().unwrap()).unwrap();
    assert_eq!(events.as_array().unwrap().len(), 1);
    assert_subset(&events[0], json!({ "name": "begin", "attributes": {} }));
    assert!(events[0]["time_ms"].is_number());
}

#[tokio::test(start_paused = true)]
async fn aggregates_canonical_series_by_interval_and_writes_partial_intervals_only_on_close() {
    let f = Fixture::at(20_001.0, Default::default()).await;
    let options = HistogramOptions { unit: Some("ms".into()), boundaries: Some(vec![1.0, 10.0]), ..Default::default() };
    let metric = f.telemetry.histogram("duration", options);
    metric.record(2.0, SpanAttributes::new().with("b", vec!["original"]).with("a", true), None);
    metric.record(8.0, SpanAttributes::new().with("a", true).with("b", vec!["original"]), None);
    f.telemetry.up_down_counter("active", InstrumentOptions::default()).add(-1.0, SpanAttributes::new());
    f.flush().await;
    assert_eq!(f.db.rows("SELECT * FROM telemetry_metrics").await, json!([]));
    f.set_now(30_001.0);
    metric.record(4.0, SpanAttributes::new().with("b", vec!["original"]).with("a", true), None);
    f.flush().await;
    let rows = f.db.rows("SELECT * FROM telemetry_metrics ORDER BY name").await;
    assert_eq!(rows.as_array().unwrap().len(), 2);
    let active =
        json!({ "name": "active", "kind": "upDownCounter", "unit": null, "count": 1, "sum": -1, "min": -1, "max": -1 });
    assert_subset(&rows[0], active);
    let duration = json!({
        "interval_ms": 20_000, "name": "duration", "kind": "histogram", "unit": "ms",
        "attributes": r#"{"a":true,"b":["original"]}"#, "count": 2, "sum": 10, "min": 2, "max": 8,
        "series_key": r#"["duration","histogram","ms","{\"a\":true,\"b\":[\"original\"]}"]"#,
    });
    assert_subset(&rows[1], duration);
    f.telemetry.close().await.unwrap();
    let partial = f.db.rows("SELECT count, sum FROM telemetry_metrics WHERE interval_ms = 30000").await;
    assert_eq!(partial, json!([{ "count": 1, "sum": 4 }]));
}

#[tokio::test(start_paused = true)]
async fn separates_kind_name_unit_and_caller_attributes_without_retaining_descriptions_or_boundaries() {
    let f = Fixture::new(Default::default()).await;
    let metrics = &f.telemetry;
    metrics.counter("same", InstrumentOptions::default()).add(1.0, SpanAttributes::new());
    metrics
        .counter("same", InstrumentOptions { unit: Some("By".into()), ..Default::default() })
        .add(2.0, Default::default());
    metrics.up_down_counter("same", InstrumentOptions::default()).add(3.0, SpanAttributes::new());
    metrics.counter("other", InstrumentOptions::default()).add(4.0, SpanAttributes::new());
    metrics.counter("same", InstrumentOptions::default()).add(5.0, SpanAttributes::new().with("method", "read"));
    let histogram = |description: &str, boundary: f64| HistogramOptions {
        description: Some(description.into()),
        boundaries: Some(vec![boundary]),
        ..Default::default()
    };
    metrics.histogram("same", histogram("first", 1.0)).record(6.0, SpanAttributes::new(), None);
    metrics.histogram("same", histogram("second", 2.0)).record(7.0, SpanAttributes::new(), None);
    f.telemetry.close().await.unwrap();
    let rows = f.db.rows("SELECT * FROM telemetry_metrics").await;
    assert_eq!(rows.as_array().unwrap().len(), 6);
    let histogram = rows.as_array().unwrap().iter().find(|row| row["kind"] == "histogram").unwrap();
    assert_subset(histogram, json!({ "count": 2, "sum": 13, "min": 6, "max": 7 }));
}

#[tokio::test(start_paused = true)]
async fn strictly_bounds_queued_spans_including_the_in_flight_batch_and_bounds_pending_metric_series() {
    let f = Fixture::new(SqliteTelemetryOptions { max_queued_spans: Some(1), ..Default::default() }).await;
    root(&f, "first").await;
    add(&f, "first", 1.0);
    add(&f, "first", 2.0);
    add(&f, "overflow", 3.0);
    let (entered, release) = f.db.hold();
    let flushing = f.telemetry.flush();
    entered.notified().await;
    root(&f, "overflow").await;
    assert_eq!((f.telemetry.dropped_spans(), f.telemetry.dropped_measurements()), (1, 1));
    release.send(true).unwrap();
    flushing.await.unwrap();
    root(&f, "after").await;
    f.telemetry.close().await.unwrap();
    let names = f.db.rows("SELECT name FROM telemetry_spans ORDER BY rowid").await;
    assert_eq!(names, json!([{ "name": "first" }, { "name": "after" }]));
    let metrics = f.db.rows("SELECT name, count, sum FROM telemetry_metrics").await;
    assert_eq!(metrics, json!([{ "name": "first", "count": 2, "sum": 3 }]));
}

#[tokio::test(start_paused = true)]
async fn counts_in_flight_metric_intervals_against_capacity_until_commit() {
    let f = Fixture::at(20_001.0, SqliteTelemetryOptions { max_queued_spans: Some(1), ..Default::default() }).await;
    add(&f, "calls", 1.0);
    f.set_now(30_001.0);
    let (entered, release) = f.db.hold();
    let flushing = f.telemetry.flush();
    entered.notified().await;
    add(&f, "calls", 2.0);
    assert_eq!(f.telemetry.dropped_measurements(), 1);
    release.send(true).unwrap();
    flushing.await.unwrap();
    add(&f, "calls", 3.0);
    f.telemetry.close().await.unwrap();
    let rows = f.db.rows("SELECT interval_ms, sum FROM telemetry_metrics ORDER BY interval_ms").await;
    assert_eq!(rows, json!([{ "interval_ms": 20_000, "sum": 1 }, { "interval_ms": 30_000, "sum": 3 }]));
}

#[tokio::test(start_paused = true)]
async fn flushes_once_per_second_and_retains_only_rows_inside_the_retention_window() {
    let f = Fixture::at(20_001.0, SqliteTelemetryOptions { retention_ms: Some(20_000.0), ..Default::default() }).await;
    root(&f, "old").await;
    add(&f, "old", 1.0);
    tokio::time::sleep(Duration::from_millis(1001)).await;
    assert_eq!(f.db.rows("SELECT name FROM telemetry_spans").await, json!([{ "name": "old" }]));
    f.set_now(50_001.0);
    root(&f, "recent").await;
    add(&f, "recent", 1.0);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(f.db.rows("SELECT name FROM telemetry_spans").await, json!([{ "name": "recent" }]));
    assert_eq!(f.db.rows("SELECT name FROM telemetry_metrics").await, json!([]));
    f.telemetry.close().await.unwrap();
    assert_eq!(f.db.rows("SELECT name FROM telemetry_metrics").await, json!([{ "name": "recent" }]));
}

#[tokio::test(start_paused = true)]
async fn serializes_concurrent_flush_and_close_without_losing_measurements_recorded_during_writes() {
    let f = Fixture::at(20_001.0, Default::default()).await;
    add(&f, "calls", 1.0);
    f.set_now(30_001.0);
    let (entered, release) = f.db.hold();
    let first = f.telemetry.flush();
    entered.notified().await;
    add(&f, "calls", 2.0);
    let second = f.telemetry.flush();
    let close = f.telemetry.close();
    assert!(close.ptr_eq(&f.telemetry.close()));
    release.send(true).unwrap();
    let (first, second, close) = tokio::join!(first, second, close);
    first.and(second).and(close).unwrap();
    assert_eq!(f.db.maximum.load(std::sync::atomic::Ordering::SeqCst), 1);
    let rows = f.db.rows("SELECT interval_ms, sum FROM telemetry_metrics ORDER BY interval_ms").await;
    assert_eq!(rows, json!([{ "interval_ms": 20_000, "sum": 1 }, { "interval_ms": 30_000, "sum": 2 }]));
}

#[tokio::test(start_paused = true)]
async fn close_does_not_wait_for_active_callbacks_ignores_late_completions_and_leaves_the_database_open() {
    let f = Fixture::new(Default::default()).await;
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let slot = Slot::default();
    let active = slot.clone();
    let pending = f.telemetry.start_infallible_span(SpanOptions::new("active"), move |span| {
        *active.lock().unwrap() = Some(span);
        async move {
            let _ = gate.await;
        }
    });
    root(&f, "settled").await;
    f.telemetry.close().await.unwrap();
    assert_eq!(f.db.rows("SELECT name FROM telemetry_spans").await, json!([{ "name": "settled" }]));
    let span = slot.lock().unwrap().clone().unwrap();
    span.add_event("late", SpanAttributes::new());
    span.start_infallible_span(SpanOptions::new("late-child"), |_| async {}).await;
    add(&f, "late", 1.0);
    release.send(()).unwrap();
    pending.await;
    f.flush().await;
    assert_eq!(f.db.rows("SELECT name FROM telemetry_spans").await, json!([{ "name": "settled" }]));
    assert_eq!(f.db.rows("SELECT * FROM telemetry_metrics").await, json!([]));
}
