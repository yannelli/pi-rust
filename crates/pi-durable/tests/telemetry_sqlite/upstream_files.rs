//! Reads and extends a database written by the upstream TypeScript recorder (`fixtures/telemetry/generate.ts`).

use std::sync::Arc;

use pi_durable::storage::sqlite::{BundledSqliteDatabase, SqliteExecutor, SqliteStorageOptions};
use pi_durable::telemetry_sqlite::{SqliteTelemetry, SqliteTelemetryOptions, create_sqlite_telemetry};
use pi_telemetry::{
    HistogramOptions, InstrumentOptions, MetricQuery, SpanAttributes, SpanErrorInfo, SpanOptions, SpanStatus,
    StoredLink, StoredMetric, TelemetryCarrier, TelemetryContextExt, TelemetryMetrics, TelemetryPropagation,
    TelemetryStore, TraceQuery,
};

use crate::support::clock;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/telemetry/telemetry.sqlite");

async fn open(directory: &tempfile::TempDir) -> (Arc<BundledSqliteDatabase>, SqliteTelemetry) {
    let path = directory.path().join("telemetry.sqlite");
    std::fs::copy(FIXTURE, &path).unwrap();
    let options = SqliteStorageOptions::default();
    let db = Arc::new(BundledSqliteDatabase::open(path.to_str().unwrap(), options).await.unwrap());
    let options = SqliteTelemetryOptions { clock: Some(clock(20_001.0).0), ..Default::default() };
    (db.clone(), create_sqlite_telemetry(db, options).await.unwrap())
}

async fn metric(telemetry: &SqliteTelemetry, name: &str) -> Vec<StoredMetric> {
    telemetry.find_metrics(MetricQuery { name: name.into(), ..Default::default() }).await.unwrap()
}

fn query(attributes: SpanAttributes) -> TraceQuery {
    TraceQuery { attributes, ..TraceQuery::default() }
}

#[tokio::test]
async fn reads_spans_links_statuses_and_metrics_written_by_upstream() {
    let directory = tempfile::tempdir().unwrap();
    let (_db, telemetry) = open(&directory).await;
    let page = telemetry.find_traces(TraceQuery::default()).await.unwrap();
    let names: Vec<Vec<&str>> =
        page.traces.iter().map(|trace| trace.spans.iter().map(|span| span.name.as_str()).collect()).collect();
    assert_eq!(names, [vec!["request", "query", "remote"], vec!["linked"]]);
    let (request, linked) = (&page.traces[0], &page.traces[1]);
    let [root, query_span, remote] = [&request.spans[0], &request.spans[1], &request.spans[2]];
    assert_eq!((root.trace_id.as_str(), root.parent_id.as_ref()), (root.id.as_str(), None));
    assert_eq!((query_span.parent_id.as_ref(), remote.parent_id.as_ref()), (Some(&root.id), Some(&root.id)));
    let attributes = SpanAttributes::new()
        .with("route", "/a")
        .with("count", 3)
        .with("ok", true)
        .with("ratio", 0.25)
        .with("tags", vec!["x", "y"])
        .with("10", "ten")
        .with("2", "two");
    assert_eq!(root.attributes, attributes);
    assert_eq!((root.started_ms, root.ended_ms >= root.started_ms), (20_001.0, true));
    assert_eq!((root.events[0].name.as_str(), &root.events[0].attributes), ("start", &[("step", 1)].into()));
    let error = SpanErrorInfo { name: "TypeError".into(), message: "bad input".into() };
    assert_eq!(query_span.status, SpanStatus::Error { error: Some(error) });
    assert_eq!(remote.status, SpanStatus::Error { error: None });
    let link = StoredLink {
        trace_id: request.trace_id.clone(),
        span_id: root.id.clone(),
        attributes: Some([("reason", "retry")].into()),
    };
    assert_eq!(linked.spans[0].links, [link]);

    let errors = TraceQuery { errors: true, ..TraceQuery::default() };
    assert_eq!(telemetry.find_traces(errors).await.unwrap().traces.len(), 1);
    let typed = query([("ok", true)].into());
    assert_eq!(telemetry.find_traces(typed).await.unwrap().traces[0].trace_id, request.trace_id);
    assert!(telemetry.find_traces(query([("ok", 1)].into())).await.unwrap().traces.is_empty());
    let tags = telemetry.find_traces(query(SpanAttributes::new().with("tags", vec!["x", "y"]))).await.unwrap();
    assert_eq!(tags.traces.len(), 1);

    let hits = metric(&telemetry, "hits").await;
    assert_eq!(hits.len(), 1);
    let read = SpanAttributes::new().with("method", "read").with("valid", true);
    assert_eq!((hits[0].interval_ms, hits[0].unit.as_deref(), &hits[0].attributes), (20_000.0, Some("By"), &read));
    assert_eq!((hits[0].count, hits[0].sum, hits[0].min, hits[0].max), (2, 5.0, 2.0, 3.0));
    let latency = SpanAttributes::new().with("b", 1).with("a", "x").with("10", "ten").with("2", "two");
    assert_eq!(metric(&telemetry, "latency").await[0].attributes, latency);
}

#[tokio::test]
async fn extends_upstream_traces_and_merges_into_upstream_metric_series() {
    let directory = tempfile::tempdir().unwrap();
    let (db, telemetry) = open(&directory).await;
    let request = telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let carrier = TelemetryCarrier::from([
        ("pi-sqlite-span".into(), request.spans[0].id.clone()),
        ("pi-sqlite-trace".into(), request.trace_id.clone()),
    ]);
    let parent = telemetry.extract(Some(&carrier));
    parent.start_infallible_span(SpanOptions::new("rust-child"), |_| async {}).await;
    let by = InstrumentOptions { unit: Some("By".into()), ..Default::default() };
    telemetry.counter("hits", by).add(4.0, SpanAttributes::new().with("valid", true).with("method", "read"));
    let ms = HistogramOptions { unit: Some("ms".into()), ..Default::default() };
    let latency = SpanAttributes::new().with("2", "two").with("a", "x").with("10", "ten").with("b", 1);
    telemetry.histogram("latency", ms).record(2.5, latency, None);
    telemetry.up_down_counter("active", InstrumentOptions::default()).add(1.0, SpanAttributes::new());
    telemetry.close().await.unwrap();

    let trace = telemetry.get_trace(&request.trace_id).await.unwrap().unwrap();
    let child = trace.spans.iter().find(|span| span.name == "rust-child").unwrap();
    assert_eq!(child.parent_id.as_ref(), Some(&request.spans[0].id));
    assert_eq!(trace.spans.len(), 4);
    let summary = |rows: Vec<StoredMetric>| rows.iter().map(|row| (row.count, row.sum)).collect::<Vec<_>>();
    assert_eq!(summary(metric(&telemetry, "hits").await), [(3, 9.0)]);
    assert_eq!(summary(metric(&telemetry, "latency").await), [(2, 4.0)]);
    assert_eq!(summary(metric(&telemetry, "active").await), [(2, 0.0)]);
    let schema = db.all("SELECT version FROM telemetry_schema", &[]).await.unwrap();
    assert_eq!(schema[0].integer("version").unwrap(), 1);
}
