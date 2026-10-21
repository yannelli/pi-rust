//! Port of upstream `describe("SQLite telemetry reads")` in `test/telemetry-sqlite-store.test.ts`.

use std::sync::atomic::Ordering;

use pi_durable::storage::sqlite::SqliteValue;
use pi_telemetry::{
    InstrumentOptions, MetricQuery, SpanOptions, TelemetryContextExt, TelemetryMetrics, TelemetryStore, TraceQuery,
};

use crate::support::Fixture;

fn limit(limit: usize) -> TraceQuery {
    TraceQuery { limit: Some(limit), ..TraceQuery::default() }
}

#[tokio::test(start_paused = true)]
async fn uses_two_span_queries_for_a_page_not_one_query_per_trace() {
    let f = Fixture::new(Default::default()).await;
    for _ in 0..25 {
        f.telemetry.start_infallible_span(SpanOptions::new("root"), |_| async {}).await;
    }
    f.flush().await;
    f.db.queries.store(0, Ordering::SeqCst);
    assert_eq!(f.telemetry.find_traces(limit(20)).await.unwrap().traces.len(), 20);
    assert_eq!(f.db.queries.swap(0, Ordering::SeqCst), 2);
    assert_eq!(f.telemetry.find_traces(limit(1)).await.unwrap().traces.len(), 1);
    assert_eq!(f.db.queries.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn reads_committed_rows_without_implicitly_flushing_including_after_close() {
    let f = Fixture::new(Default::default()).await;
    f.telemetry.start_infallible_span(SpanOptions::new("pending"), |_| async {}).await;
    assert!(f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.is_empty());
    f.telemetry.counter("partial", InstrumentOptions::default()).add(2.0, Default::default());
    let partial = || MetricQuery { name: "partial".into(), ..Default::default() };
    assert!(f.telemetry.find_metrics(partial()).await.unwrap().is_empty());
    f.telemetry.close().await.unwrap();
    let trace = f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let stored = f.telemetry.get_trace(&trace.trace_id).await.unwrap().unwrap();
    assert_eq!(stored.spans[0].name, "pending");
    let metrics = f.telemetry.find_metrics(partial()).await.unwrap();
    assert_eq!(metrics.iter().map(|row| (row.count, row.sum)).collect::<Vec<_>>(), [(1, 2.0)]);
}

#[tokio::test(start_paused = true)]
async fn keeps_surviving_descendants_discoverable_after_a_stored_parent_is_removed() {
    let f = Fixture::new(Default::default()).await;
    f.telemetry
        .start_infallible_span(SpanOptions::new("root"), |root| async move {
            root.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
        })
        .await;
    f.flush().await;
    let original = f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let parent = original.spans.iter().find(|span| span.name == "root").unwrap();
    f.db.run("DELETE FROM telemetry_spans WHERE id = ?", &[SqliteValue::from(parent.id.as_str())]).await;
    let trace = f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    assert_eq!(trace.trace_id, original.trace_id);
    assert_eq!(trace.spans.len(), 1);
    assert_eq!((trace.spans[0].name.as_str(), trace.spans[0].parent_id.as_ref()), ("child", Some(&parent.id)));
    let stored = f.telemetry.get_trace(&original.trace_id).await.unwrap().unwrap();
    assert_eq!(stored.spans.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn rejects_an_unreadable_cursor_and_a_zero_limit() {
    let f = Fixture::new(Default::default()).await;
    f.telemetry.start_infallible_span(SpanOptions::new("root"), |_| async {}).await;
    f.flush().await;
    let bad = TraceQuery { cursor: Some("not json".into()), ..TraceQuery::default() };
    assert!(f.telemetry.find_traces(bad).await.is_err());
    assert!(f.telemetry.find_traces(limit(0)).await.is_err());
}
