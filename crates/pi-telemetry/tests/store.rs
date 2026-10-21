//! Port of upstream `test/store.test.ts` and `test/store-conformance.ts` for the in-memory store. The wall
//! clock is a settable value, as upstream mocks `Date.now`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_telemetry::{
    InMemoryTelemetryContext, InstrumentOptions, MetricQuery, RecordedTelemetryEvent, RecordedTelemetryMeasurement,
    SpanAttributes, SpanLink, SpanOptions, SpanStatus, StoredEvent, StoredLink, StoredSpan, StoredTrace,
    TelemetryCarrier, TelemetryClock, TelemetryContextExt, TelemetryMetricKind, TelemetryMetrics, TelemetryPropagation,
    TelemetryStore, TraceQuery,
};

type Time = Arc<Mutex<f64>>;

fn set(time: &Time, value: f64) {
    *time.lock().unwrap() = value;
}

/// A recorder whose wall clock is `now`; durations come from the real monotonic clock.
fn recorder(start: f64) -> (InMemoryTelemetryContext, Time) {
    let now: Time = Arc::new(Mutex::new(start));
    let (wall, origin) = (now.clone(), Instant::now());
    let clock = TelemetryClock::new(move || *wall.lock().unwrap(), move || origin.elapsed().as_secs_f64() * 1000.0);
    (InMemoryTelemetryContext::with_clock(clock), now)
}

async fn root(context: &InMemoryTelemetryContext, name: &str) {
    context.start_infallible_span(SpanOptions::new(name), |_| async {}).await;
}

async fn names(store: &InMemoryTelemetryContext, query: TraceQuery) -> Vec<String> {
    let page = store.find_traces(query).await.unwrap();
    page.traces.iter().map(|trace| trace.spans[0].name.clone()).collect()
}

fn attrs(entries: impl Into<SpanAttributes>) -> SpanAttributes {
    entries.into()
}

#[tokio::test]
async fn returns_only_settled_traces_complete_span_snapshots_and_detached_results() {
    let (context, _) = recorder(20_000.0);
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let options = SpanOptions::new("root").with_attributes(SpanAttributes::new().with("tags", vec!["original"]));
    let mut active = Box::pin(context.start_infallible_span(options, |root| async move {
        root.add_event("event", SpanAttributes::new().with("tags", vec!["original"]));
        let _ = gate.await;
        root.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
    }));
    assert!(futures::poll!(active.as_mut()).is_pending());
    assert!(context.find_traces(TraceQuery::default()).await.unwrap().traces.is_empty());
    assert_eq!(context.get_trace("missing").await.unwrap(), None);
    release.send(()).unwrap();
    active.await;

    let trace = context.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let root = trace.spans.iter().find(|span| span.name == "root").unwrap();
    let child = trace.spans.iter().find(|span| span.name == "child").unwrap();
    let (scope, counter) = root.id.split_once(':').unwrap();
    let hex = scope.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    assert!(scope.len() == 36 && hex && counter.parse::<u64>().unwrap() >= 1, "{}", root.id);
    assert_eq!(root.trace_id, root.id);
    assert_eq!(root.parent_id, None);
    assert_eq!(child.parent_id.as_ref(), Some(&root.id));
    assert_eq!(child.trace_id, root.trace_id);
    assert_eq!(root.started_ms, 20_000.0);
    assert!(root.ended_ms >= root.started_ms);
    assert!(root.events[0].time_ms >= 0.0);

    let mut changed = root.clone();
    changed.attributes.set("tags", vec!["changed"]);
    changed.events[0].attributes.set("tags", vec!["changed"]);
    let fresh = context.get_trace(&trace.trace_id).await.unwrap().unwrap();
    let again = fresh.spans.iter().find(|span| span.id == root.id).unwrap();
    assert_eq!(again.attributes.get("tags"), Some(&vec!["original"].into()));
    assert_eq!(again.events[0].attributes.get("tags"), Some(&vec!["original"].into()));
}

#[tokio::test]
async fn filters_by_first_span_name_and_start_any_error_and_all_attributes_on_one_span() {
    let (context, now) = recorder(10_000.0);
    let clock = now.clone();
    context
        .start_infallible_span(SpanOptions::new("old"), |old| async move {
            set(&clock, 30_000.0);
            let recent = SpanOptions::new("recent-child").with_attributes(attrs([("left", 1)]));
            old.start_infallible_span(recent, |_| async {}).await;
            let split = SpanOptions::new("split").with_attributes(attrs([("right", 2)]));
            old.start_infallible_span(split, |_| async {}).await;
        })
        .await;
    set(&now, 20_000.0);
    let clock = now.clone();
    context
        .start_infallible_span(SpanOptions::new("wanted"), |wanted| async move {
            set(&clock, 20_001.0);
            let both =
                SpanAttributes::new().with("left", 1).with("right", 2).with("valid", true).with("odd.\"key", "value");
            let options = SpanOptions::new("failure").with_attributes(both);
            wanted
                .start_infallible_span(options, |failure| async move {
                    failure.set_status(SpanStatus::Error { error: None })
                })
                .await;
        })
        .await;
    set(&now, 40_000.0);
    let new = SpanOptions::new("new").with_attributes(attrs([("valid", 1)]));
    context.start_infallible_span(new, |_| async {}).await;

    let wanted = TraceQuery {
        name: Some("wanted".into()),
        attributes: attrs([("left", 1), ("right", 2)]),
        errors: true,
        start_min: Some(20_000.0),
        start_max: Some(20_000.0),
        ..TraceQuery::default()
    };
    assert_eq!(names(&context, wanted).await, ["wanted"]);
    let by_name = TraceQuery { name: Some("failure".into()), ..TraceQuery::default() };
    assert!(names(&context, by_name).await.is_empty());
    let query = |attributes: SpanAttributes| TraceQuery { attributes, ..TraceQuery::default() };
    assert_eq!(names(&context, query(attrs([("left", 1), ("right", 2)]))).await, ["wanted"]);
    let odd = SpanAttributes::new().with("valid", true).with("odd.\"key", "value");
    assert_eq!(names(&context, query(odd)).await, ["wanted"]);
    assert_eq!(names(&context, query(attrs([("valid", 1)]))).await, ["new"]);
    assert_eq!(names(&context, TraceQuery { errors: true, ..TraceQuery::default() }).await, ["wanted"]);
    assert_eq!(
        names(&context, TraceQuery { start_min: Some(20_000.0), ..TraceQuery::default() }).await,
        ["new", "wanted"]
    );
    assert_eq!(names(&context, TraceQuery { start_max: Some(10_000.0), ..TraceQuery::default() }).await, ["old"]);
}

#[tokio::test]
async fn pages_complete_traces_without_duplicates_when_trace_starts_tie() {
    let (context, now) = recorder(20_000.0);
    for index in 0..25 {
        set(&now, 20_000.0);
        let clock = now.clone();
        let options = SpanOptions::new("root").with_attributes(attrs([("index", index)]));
        context
            .start_infallible_span(options, |root| async move {
                set(&clock, 20_001.0);
                root.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
            })
            .await;
    }
    let first = context.find_traces(TraceQuery::default()).await.unwrap();
    assert_eq!(first.traces.len(), 20);
    assert!(first.cursor.is_some());
    let rest = context.find_traces(TraceQuery { cursor: first.cursor.clone(), ..TraceQuery::default() }).await.unwrap();
    assert_eq!((rest.traces.len(), rest.cursor), (5, None));
    let all = context.find_traces(TraceQuery { limit: Some(100), ..TraceQuery::default() }).await.unwrap().traces;
    let joined: Vec<StoredTrace> = first.traces.into_iter().chain(rest.traces).collect();
    assert_eq!(joined, all);
    assert!(all.iter().all(|trace| trace.spans.len() == 2));

    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let query =
            TraceQuery { name: Some("root".into()), limit: Some(3), cursor: cursor.clone(), ..Default::default() };
        let page = context.find_traces(query).await.unwrap();
        ids.extend(page.traces.iter().map(|trace| trace.trace_id.clone()));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, all.iter().map(|trace| trace.trace_id.clone()).collect::<Vec<_>>());
}

#[tokio::test]
async fn inherits_remote_trace_ids_and_preserves_remote_parents_and_cross_trace_links() {
    let (sender, _) = recorder(20_000.0);
    let (receiver, _) = recorder(20_000.0);
    let sent_by = sender.clone();
    let carrier = sender
        .start_infallible_span(SpanOptions::new("sender"), |span| async move { sent_by.inject(span.as_ref()).unwrap() })
        .await;
    let sent = sender.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let wire = serde_json::to_string(&carrier).unwrap();
    let remote = receiver.extract(Some(&serde_json::from_str::<TelemetryCarrier>(&wire).unwrap()));
    remote.start_infallible_span(SpanOptions::new("receiver"), |_| async {}).await;
    let link = SpanLink { context: remote.clone(), attributes: attrs([("reason", "background")]) };
    let linked = SpanOptions::new("linked").with_links(vec![link]);
    receiver.start_infallible_span(linked, |_| async {}).await;

    let continued = receiver.get_trace(&sent.trace_id).await.unwrap().unwrap();
    assert_eq!(continued.spans.len(), 1);
    let span = &continued.spans[0];
    assert_eq!(
        (&span.trace_id, span.parent_id.as_ref(), span.name.as_str()),
        (&sent.trace_id, Some(&sent.spans[0].id), "receiver")
    );
    assert_ne!(span.id, sent.spans[0].id);
    let query = TraceQuery { name: Some("linked".into()), ..TraceQuery::default() };
    let linked = receiver.find_traces(query).await.unwrap().traces.remove(0);
    assert_ne!(linked.trace_id, sent.trace_id);
    let expected = StoredLink {
        trace_id: sent.trace_id.clone(),
        span_id: sent.spans[0].id.clone(),
        attributes: Some(attrs([("reason", "background")])),
    };
    assert_eq!(linked.spans[0].links, [expected]);

    for key in carrier.keys() {
        let mut incomplete = carrier.clone();
        incomplete.remove(key);
        receiver.extract(Some(&incomplete)).start_infallible_span(SpanOptions::new("fallback"), |_| async {}).await;
    }
    let query = TraceQuery { name: Some("fallback".into()), ..TraceQuery::default() };
    let fallbacks = receiver.find_traces(query).await.unwrap().traces;
    assert_eq!(fallbacks.len(), carrier.len());
    for trace in &fallbacks {
        assert_eq!((trace.spans[0].parent_id.as_ref(), &trace.trace_id), (None, &trace.spans[0].id));
    }
}

#[tokio::test]
async fn accepts_external_string_identities_without_imposing_an_id_format() {
    let (context, _) = recorder(20_000.0);
    let injector = context.clone();
    let carrier = context
        .start_infallible_span(
            SpanOptions::new("source"),
            |span| async move { injector.inject(span.as_ref()).unwrap() },
        )
        .await;
    let external: TelemetryCarrier = carrier.keys().map(|key| (key.clone(), format!("external-{key}"))).collect();
    let parent = context.extract(Some(&external));
    assert_eq!(context.inject(parent.as_ref()), Some(external));
    parent.start_infallible_span(SpanOptions::new("continued"), |_| async {}).await;
    let query = TraceQuery { name: Some("continued".into()), ..TraceQuery::default() };
    let trace = context.find_traces(query).await.unwrap().traces.remove(0);
    assert!(trace.trace_id.starts_with("external-"));
    assert!(trace.spans[0].parent_id.as_ref().unwrap().starts_with("external-"));
}

#[tokio::test]
async fn queries_metric_names_inclusive_interval_bounds_typed_attribute_equality_and_detached_values() {
    let (context, now) = recorder(20_001.0);
    let counter = context.counter("hits", InstrumentOptions { unit: Some("By".into()), ..Default::default() });
    counter.add(2.0, SpanAttributes::new().with("method", "read").with("valid", true));
    counter.add(3.0, SpanAttributes::new().with("valid", true).with("method", "read"));
    counter.add(11.0, SpanAttributes::new().with("method", "write").with("valid", 1));
    context.counter("other", InstrumentOptions::default()).add(100.0, attrs([("method", "read")]));
    set(&now, 30_001.0);
    counter.add(7.0, SpanAttributes::new().with("method", "read").with("valid", true));
    set(&now, 40_001.0);

    let query = MetricQuery {
        name: "hits".into(),
        start_min: Some(20_000.0),
        start_max: Some(20_000.0),
        attributes: SpanAttributes::new().with("method", "read").with("valid", true),
    };
    let mut rows = context.find_metrics(query.clone()).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.count).sum::<u64>(), 2);
    assert_eq!(rows.iter().map(|row| row.sum).sum::<f64>(), 5.0);
    assert_eq!(rows.iter().map(|row| row.min).fold(f64::INFINITY, f64::min), 2.0);
    assert_eq!(rows.iter().map(|row| row.max).fold(f64::NEG_INFINITY, f64::max), 3.0);
    let counter_rows =
        rows.iter().all(|row| row.kind == TelemetryMetricKind::Counter && row.unit.as_deref() == Some("By"));
    assert!(counter_rows && rows.iter().all(|row| row.interval_ms == 20_000.0));
    rows[0].attributes.set("method", "changed");
    assert_eq!(context.find_metrics(query).await.unwrap()[0].attributes.get("method"), Some(&"read".into()));
    assert!(
        context.find_metrics(MetricQuery { name: "missing".into(), ..Default::default() }).await.unwrap().is_empty()
    );
    let sum = |rows: Vec<pi_telemetry::StoredMetric>| rows.iter().map(|row| row.sum).sum::<f64>();
    let typed = MetricQuery { name: "hits".into(), attributes: attrs([("valid", 1)]), ..Default::default() };
    assert_eq!(sum(context.find_metrics(typed).await.unwrap()), 11.0);
    let later = MetricQuery { name: "hits".into(), start_min: Some(30_000.0), ..Default::default() };
    assert_eq!(sum(context.find_metrics(later).await.unwrap()), 7.0);
}

#[tokio::test]
async fn records_wall_clock_boundaries_and_relative_event_times() {
    let (wall, tick): (Time, Time) = (Arc::new(Mutex::new(1000.0)), Arc::new(Mutex::new(100.0)));
    let (read_wall, read_tick) = (wall.clone(), tick.clone());
    let clock = TelemetryClock::new(move || *read_wall.lock().unwrap(), move || *read_tick.lock().unwrap());
    let telemetry = InMemoryTelemetryContext::with_clock(clock);
    let recorder = telemetry.clone();
    let id = telemetry
        .start_infallible_span(SpanOptions::new("timed"), |span| async move {
            let carrier = recorder.inject(span.as_ref()).unwrap();
            set(&tick, 110.0);
            span.add_event("event", SpanAttributes::new().with("count", 1));
            set(&tick, 125.0);
            format!("{}:{}", carrier["pi-memory-scope"], carrier["pi-memory-span"])
        })
        .await;
    let expected = StoredSpan {
        id: id.clone(),
        trace_id: id.clone(),
        parent_id: None,
        name: "timed".into(),
        started_ms: 1000.0,
        ended_ms: 1025.0,
        attributes: SpanAttributes::new(),
        events: vec![StoredEvent { name: "event".into(), time_ms: 10.0, attributes: attrs([("count", 1)]) }],
        links: Vec::new(),
        status: SpanStatus::Ok,
    };
    assert_eq!(telemetry.get_trace(&id).await.unwrap(), Some(StoredTrace { trace_id: id, spans: vec![expected] }));
    let event = RecordedTelemetryEvent { name: "event".into(), attributes: attrs([("count", 1)]) };
    assert_eq!(telemetry.get_spans()[0].events, [event]);
}

#[tokio::test]
async fn maps_raw_metric_measurements_to_count_one_rows_without_exposing_timing_in_get_metrics() {
    let (telemetry, _) = recorder(20_001.0);
    telemetry.counter("calls", InstrumentOptions::default()).add(2.0, SpanAttributes::new());
    telemetry.counter("calls", InstrumentOptions::default()).add(3.0, SpanAttributes::new());
    let rows = telemetry.find_metrics(MetricQuery { name: "calls".into(), ..Default::default() }).await.unwrap();
    let summary: Vec<(f64, u64, f64)> = rows.iter().map(|row| (row.interval_ms, row.count, row.sum)).collect();
    assert_eq!(summary, [(20_000.0, 1, 2.0), (20_000.0, 1, 3.0)]);
    let measured = |value: f64| RecordedTelemetryMeasurement {
        kind: TelemetryMetricKind::Counter,
        name: "calls".into(),
        description: None,
        unit: None,
        boundaries: None,
        value,
        attributes: SpanAttributes::new(),
        context: None,
    };
    assert_eq!(telemetry.get_metrics(), [measured(2.0), measured(3.0)]);
}

#[tokio::test]
async fn detaches_link_attributes_and_error_details() {
    let (telemetry, _) = recorder(1.0);
    let target = telemetry.start_infallible_span(SpanOptions::new("target"), |span| async move { span }).await;
    let link = SpanLink { context: target, attributes: SpanAttributes::new().with("tags", vec!["original"]) };
    let options = SpanOptions::new("source").with_links(vec![link]);
    telemetry
        .start_infallible_span(
            options,
            |span| async move { span.set_status(SpanStatus::error("Expected", "original")) },
        )
        .await;
    let query = TraceQuery { name: Some("source".into()), ..TraceQuery::default() };
    let mut trace = telemetry.find_traces(query).await.unwrap().traces.remove(0);
    trace.spans[0].links[0].attributes = Some(attrs([("tags", "changed")]));
    trace.spans[0].status = SpanStatus::error("Expected", "changed");
    let fresh = telemetry.get_trace(&trace.trace_id).await.unwrap().unwrap();
    let tags = SpanAttributes::new().with("tags", vec!["original"]);
    assert_eq!(fresh.spans[0].links[0].attributes, Some(tags));
    assert_eq!(fresh.spans[0].status, SpanStatus::error("Expected", "original"));
}

#[tokio::test]
async fn rejects_an_unreadable_cursor_and_a_zero_limit() {
    let (telemetry, _) = recorder(1.0);
    root(&telemetry, "root").await;
    let bad = TraceQuery { cursor: Some("not json".into()), ..TraceQuery::default() };
    assert!(telemetry.find_traces(bad).await.is_err());
    let zero = TraceQuery { limit: Some(0), ..TraceQuery::default() };
    assert!(telemetry.find_traces(zero).await.is_err());
}
