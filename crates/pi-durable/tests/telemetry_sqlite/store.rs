//! Upstream `telemetryStoreConformance` (pi-telemetry `test/store-conformance.ts`) over the SQLite store.

use pi_telemetry::{
    InstrumentOptions, MetricQuery, SpanAttributes, SpanLink, SpanOptions, SpanStatus, StoredLink, StoredMetric,
    StoredTrace, TelemetryCarrier, TelemetryContextExt, TelemetryMetricKind, TelemetryMetrics, TelemetryPropagation,
    TelemetryStore, TraceQuery,
};

use crate::support::{Fixture, set};

fn attrs(entries: impl Into<SpanAttributes>) -> SpanAttributes {
    entries.into()
}

async fn names(f: &Fixture, query: TraceQuery) -> Vec<String> {
    let page = f.telemetry.find_traces(query).await.unwrap();
    page.traces.iter().map(|trace| trace.spans[0].name.clone()).collect()
}

#[tokio::test(start_paused = true)]
async fn returns_only_settled_traces_complete_span_snapshots_and_detached_results() {
    let f = Fixture::at(20_000.0, Default::default()).await;
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let options = SpanOptions::new("root").with_attributes(SpanAttributes::new().with("tags", vec!["original"]));
    let mut active = Box::pin(f.telemetry.start_infallible_span(options, |root| async move {
        root.add_event("event", SpanAttributes::new().with("tags", vec!["original"]));
        let _ = gate.await;
        root.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
    }));
    assert!(futures::poll!(active.as_mut()).is_pending());
    f.flush().await;
    assert!(f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.is_empty());
    assert_eq!(f.telemetry.get_trace("missing").await.unwrap(), None);
    release.send(()).unwrap();
    active.await;
    f.flush().await;

    let trace = f.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let root = trace.spans.iter().find(|span| span.name == "root").unwrap();
    let child = trace.spans.iter().find(|span| span.name == "child").unwrap();
    let (scope, counter) = root.id.split_once(':').unwrap();
    let hex = scope.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f' | '-'));
    assert!(scope.len() == 36 && hex && !counter.starts_with('0') && counter.parse::<u64>().is_ok(), "{}", root.id);
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
    let fresh = f.telemetry.get_trace(&trace.trace_id).await.unwrap().unwrap();
    let again = fresh.spans.iter().find(|span| span.id == root.id).unwrap();
    assert_eq!(again.attributes.get("tags"), Some(&vec!["original"].into()));
    assert_eq!(again.events[0].attributes.get("tags"), Some(&vec!["original"].into()));
}

#[tokio::test(start_paused = true)]
async fn filters_by_first_span_name_and_start_any_error_and_all_attributes_on_one_span() {
    let f = Fixture::at(10_000.0, Default::default()).await;
    let now = f.now.clone().unwrap();
    let clock = now.clone();
    f.telemetry
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
    f.telemetry
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
    f.telemetry.start_infallible_span(new, |_| async {}).await;
    f.flush().await;

    let wanted = TraceQuery {
        name: Some("wanted".into()),
        attributes: attrs([("left", 1), ("right", 2)]),
        errors: true,
        start_min: Some(20_000.0),
        start_max: Some(20_000.0),
        ..TraceQuery::default()
    };
    assert_eq!(names(&f, wanted).await, ["wanted"]);
    assert!(names(&f, TraceQuery { name: Some("failure".into()), ..TraceQuery::default() }).await.is_empty());
    let query = |attributes: SpanAttributes| TraceQuery { attributes, ..TraceQuery::default() };
    assert_eq!(names(&f, query(attrs([("left", 1), ("right", 2)]))).await, ["wanted"]);
    let odd = SpanAttributes::new().with("valid", true).with("odd.\"key", "value");
    assert_eq!(names(&f, query(odd)).await, ["wanted"]);
    assert_eq!(names(&f, query(attrs([("valid", 1)]))).await, ["new"]);
    assert_eq!(names(&f, TraceQuery { errors: true, ..TraceQuery::default() }).await, ["wanted"]);
    let recent = TraceQuery { start_min: Some(20_000.0), ..TraceQuery::default() };
    assert_eq!(names(&f, recent).await, ["new", "wanted"]);
    assert_eq!(names(&f, TraceQuery { start_max: Some(10_000.0), ..TraceQuery::default() }).await, ["old"]);
}

#[tokio::test(start_paused = true)]
async fn pages_complete_traces_without_duplicates_when_trace_starts_tie() {
    let f = Fixture::at(20_000.0, Default::default()).await;
    let now = f.now.clone().unwrap();
    for index in 0..25 {
        set(&now, 20_000.0);
        let clock = now.clone();
        let options = SpanOptions::new("root").with_attributes(attrs([("index", index)]));
        f.telemetry
            .start_infallible_span(options, |root| async move {
                set(&clock, 20_001.0);
                root.start_infallible_span(SpanOptions::new("child"), |_| async {}).await;
            })
            .await;
    }
    f.flush().await;
    let first = f.telemetry.find_traces(TraceQuery::default()).await.unwrap();
    assert_eq!(first.traces.len(), 20);
    assert!(first.cursor.is_some());
    let rest = TraceQuery { cursor: first.cursor.clone(), ..TraceQuery::default() };
    let rest = f.telemetry.find_traces(rest).await.unwrap();
    assert_eq!((rest.traces.len(), rest.cursor), (5, None));
    let all = TraceQuery { limit: Some(100), ..TraceQuery::default() };
    let all = f.telemetry.find_traces(all).await.unwrap().traces;
    let joined: Vec<StoredTrace> = first.traces.into_iter().chain(rest.traces).collect();
    assert_eq!(joined, all);
    assert!(all.iter().all(|trace| trace.spans.len() == 2));

    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let query =
            TraceQuery { name: Some("root".into()), limit: Some(3), cursor: cursor.clone(), ..Default::default() };
        let page = f.telemetry.find_traces(query).await.unwrap();
        ids.extend(page.traces.iter().map(|trace| trace.trace_id.clone()));
        cursor = page.cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, all.iter().map(|trace| trace.trace_id.clone()).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn inherits_remote_trace_ids_and_preserves_remote_parents_and_cross_trace_links() {
    let sender = Fixture::at(20_000.0, Default::default()).await;
    let receiver = Fixture::at(20_000.0, Default::default()).await;
    let injector = sender.telemetry.clone();
    let carrier = sender
        .telemetry
        .start_infallible_span(
            SpanOptions::new("sender"),
            |span| async move { injector.inject(span.as_ref()).unwrap() },
        )
        .await;
    sender.flush().await;
    let sent = sender.telemetry.find_traces(TraceQuery::default()).await.unwrap().traces.remove(0);
    let wire = serde_json::to_string(&carrier).unwrap();
    let remote = receiver.telemetry.extract(Some(&serde_json::from_str::<TelemetryCarrier>(&wire).unwrap()));
    remote.start_infallible_span(SpanOptions::new("receiver"), |_| async {}).await;
    let link = SpanLink { context: remote.clone(), attributes: attrs([("reason", "background")]) };
    let linked = SpanOptions::new("linked").with_links(vec![link]);
    receiver.telemetry.start_infallible_span(linked, |_| async {}).await;
    receiver.flush().await;

    let continued = receiver.telemetry.get_trace(&sent.trace_id).await.unwrap().unwrap();
    assert_eq!(continued.spans.len(), 1);
    let span = &continued.spans[0];
    assert_eq!(
        (&span.trace_id, span.parent_id.as_ref(), span.name.as_str()),
        (&sent.trace_id, Some(&sent.spans[0].id), "receiver")
    );
    assert_ne!(span.id, sent.spans[0].id);
    let query = TraceQuery { name: Some("linked".into()), ..TraceQuery::default() };
    let linked = receiver.telemetry.find_traces(query).await.unwrap().traces.remove(0);
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
        let fallback = receiver.telemetry.extract(Some(&incomplete));
        fallback.start_infallible_span(SpanOptions::new("fallback"), |_| async {}).await;
    }
    receiver.flush().await;
    let query = TraceQuery { name: Some("fallback".into()), ..TraceQuery::default() };
    let fallbacks = receiver.telemetry.find_traces(query).await.unwrap().traces;
    assert_eq!(fallbacks.len(), carrier.len());
    for trace in &fallbacks {
        assert_eq!((trace.spans[0].parent_id.as_ref(), &trace.trace_id), (None, &trace.spans[0].id));
    }
}

#[tokio::test(start_paused = true)]
async fn accepts_external_string_identities_without_imposing_an_id_format() {
    let f = Fixture::new(Default::default()).await;
    let injector = f.telemetry.clone();
    let carrier = f
        .telemetry
        .start_infallible_span(
            SpanOptions::new("source"),
            |span| async move { injector.inject(span.as_ref()).unwrap() },
        )
        .await;
    let external: TelemetryCarrier = carrier.keys().map(|key| (key.clone(), format!("external-{key}"))).collect();
    let parent = f.telemetry.extract(Some(&external));
    assert_eq!(f.telemetry.inject(parent.as_ref()), Some(external));
    parent.start_infallible_span(SpanOptions::new("continued"), |_| async {}).await;
    f.flush().await;
    let query = TraceQuery { name: Some("continued".into()), ..TraceQuery::default() };
    let trace = f.telemetry.find_traces(query).await.unwrap().traces.remove(0);
    assert!(trace.trace_id.starts_with("external-"));
    assert!(trace.spans[0].parent_id.as_ref().unwrap().starts_with("external-"));
}

#[tokio::test(start_paused = true)]
async fn queries_metric_names_inclusive_interval_bounds_typed_attribute_equality_and_detached_values() {
    let f = Fixture::at(20_001.0, Default::default()).await;
    let counter = f.telemetry.counter("hits", InstrumentOptions { unit: Some("By".into()), ..Default::default() });
    counter.add(2.0, SpanAttributes::new().with("method", "read").with("valid", true));
    counter.add(3.0, SpanAttributes::new().with("valid", true).with("method", "read"));
    counter.add(11.0, SpanAttributes::new().with("method", "write").with("valid", 1));
    let other = SpanAttributes::new().with("method", "read").with("valid", true);
    f.telemetry.counter("other", InstrumentOptions::default()).add(100.0, other);
    f.set_now(30_001.0);
    counter.add(7.0, SpanAttributes::new().with("method", "read").with("valid", true));
    f.set_now(40_001.0);
    f.flush().await;

    let query = MetricQuery {
        name: "hits".into(),
        start_min: Some(20_000.0),
        start_max: Some(20_000.0),
        attributes: SpanAttributes::new().with("method", "read").with("valid", true),
    };
    let mut rows = f.telemetry.find_metrics(query.clone()).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.count).sum::<u64>(), 2);
    assert_eq!(rows.iter().map(|row| row.sum).sum::<f64>(), 5.0);
    assert_eq!(rows.iter().map(|row| row.min).fold(f64::INFINITY, f64::min), 2.0);
    assert_eq!(rows.iter().map(|row| row.max).fold(f64::NEG_INFINITY, f64::max), 3.0);
    let counters = rows.iter().all(|row| row.kind == TelemetryMetricKind::Counter && row.unit.as_deref() == Some("By"));
    assert!(counters && rows.iter().all(|row| row.interval_ms == 20_000.0));
    rows[0].attributes.set("method", "changed");
    let fresh = f.telemetry.find_metrics(query).await.unwrap();
    assert_eq!(fresh[0].attributes.get("method"), Some(&"read".into()));
    let missing = MetricQuery { name: "missing".into(), ..Default::default() };
    assert!(f.telemetry.find_metrics(missing).await.unwrap().is_empty());
    let sum = |rows: Vec<StoredMetric>| rows.iter().map(|row| row.sum).sum::<f64>();
    let typed = MetricQuery { name: "hits".into(), attributes: attrs([("valid", 1)]), ..Default::default() };
    assert_eq!(sum(f.telemetry.find_metrics(typed).await.unwrap()), 11.0);
    let later = MetricQuery { name: "hits".into(), start_min: Some(30_000.0), ..Default::default() };
    assert_eq!(sum(f.telemetry.find_metrics(later).await.unwrap()), 7.0);
}
