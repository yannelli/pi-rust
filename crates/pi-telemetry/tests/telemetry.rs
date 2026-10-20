//! Port of upstream `test/telemetry.test.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_telemetry::{
    HistogramOptions, InMemoryTelemetryContext, InstrumentOptions, NOOP_TELEMETRY_CONTEXT, SpanAttributes, SpanLink,
    SpanOptions, SpanOutcome, SpanStatus, TelemetryCarrier, TelemetryContext, TelemetryContextExt, TelemetryMetrics,
    TelemetryPropagation, TelemetrySchemaDefinition, TelemetrySpan, create_typed_metrics, create_typed_span_starter,
    define_telemetry_schema,
};
use serde_json::json;

fn schema(value: serde_json::Value) -> TelemetrySchemaDefinition {
    define_telemetry_schema(serde_json::from_value(value).expect("valid schema"))
}

fn operation_schema() -> TelemetrySchemaDefinition {
    schema(json!({
        "version": 1,
        "spans": {"operation": {
            "description": "Operation",
            "parents": {"kind": "root_or_external"},
            "startAttributes": {"kind": {"type": "string", "required": true, "values": ["read", "write"], "description": "Kind"}},
            "endAttributes": {},
            "events": {"result": {"description": "Result", "attributes": {
                "outcome": {"type": "string", "required": true, "values": ["ok", "error"], "description": "Outcome"}
            }}},
            "status": {"default": "ok", "errorWhen": "The operation fails"}
        }}
    }))
}

fn request_schema() -> TelemetrySchemaDefinition {
    schema(json!({
        "version": 3,
        "spans": {"request": {
            "description": "Request",
            "parents": {"kind": "spans", "spans": ["operation"]},
            "startAttributes": {"provider": {"type": "string", "required": true, "description": "Provider"}},
            "endAttributes": {"response": {"type": "string", "description": "Response kind"}},
            "status": {"default": "ok", "errorWhen": "The request fails"}
        }}
    }))
}

#[test]
fn preserves_serializable_definitions() {
    let definition = operation_schema();
    let encoded = serde_json::to_value(&definition).unwrap();
    assert_eq!(serde_json::from_value::<TelemetrySchemaDefinition>(encoded.clone()).unwrap(), definition);
    assert_eq!(encoded["spans"]["operation"]["parents"], json!({"kind": "root_or_external"}));
    assert_eq!(encoded["spans"]["operation"]["status"]["errorWhen"], "The operation fails");
}

#[tokio::test]
async fn checks_span_names_attributes_and_events() {
    let context = Arc::new(InMemoryTelemetryContext::new());
    let start = create_typed_span_starter(context.clone(), &[operation_schema()]).unwrap();
    let kind = |value: &str| SpanAttributes::new().with("kind", value);
    let result = start
        .start_span("operation", kind("read"), |span, _| async move {
            span.add_event("result", SpanAttributes::new().with("outcome", "ok")).unwrap();
            assert!(span.add_event("result", SpanAttributes::new()).is_err(), "required event attributes");
            assert!(span.add_event("result", SpanAttributes::new().with("outcome", "other")).is_err());
            assert!(span.add_event("unknown", SpanAttributes::new()).is_err(), "undeclared events");
            assert!(span.set_attributes(SpanAttributes::new().with("unknown", true)).is_err());
            Ok::<_, String>(1)
        })
        .unwrap()
        .await;
    assert_eq!(result, Ok(1));
    assert!(start.start_span("operation", kind("other"), |_, _| async { Ok::<_, String>(()) }).is_err());
    assert!(start.start_span("operation", SpanAttributes::new(), |_, _| async { Ok::<_, String>(()) }).is_err());
    assert!(start.start_span("unknown", SpanAttributes::new(), |_, _| async { Ok::<_, String>(()) }).is_err());
    let extra = kind("read").with("unknown", true);
    assert!(start.start_span("operation", extra, |_, _| async { Ok::<_, String>(()) }).is_err());
    assert_eq!(context.get_spans().len(), 1, "rejected starts admit nothing");
    assert_eq!(context.get_spans()[0].events.len(), 1);
}

#[tokio::test]
async fn combines_schema_vocabularies_and_binds_child_starters_to_their_parent_spans() {
    let context = Arc::new(InMemoryTelemetryContext::new());
    let start = create_typed_span_starter(context.clone(), &[operation_schema(), request_schema()]).unwrap();
    let result = start
        .start_span("operation", SpanAttributes::new().with("kind", "read"), |_operation, child| async move {
            let request = SpanAttributes::new().with("provider", "example");
            child
                .start_span("request", request, |span, _| async move {
                    span.set_attributes(SpanAttributes::new().with("response", "cached")).unwrap();
                    Ok::<_, String>(42)
                })
                .unwrap()
                .await
        })
        .unwrap()
        .await;
    assert_eq!(result, Ok(42));
    let spans = context.get_spans();
    let operation = spans.iter().find(|span| span.name == "operation").unwrap();
    let request = spans.iter().find(|span| span.name == "request").unwrap();
    assert_eq!(operation.parent_id, None);
    assert_eq!(request.parent_id, Some(operation.id));

    let sync = start
        .start_span("operation", SpanAttributes::new().with("kind", "write"), |_, _| async { Err::<(), _>("sync") });
    assert_eq!(sync.unwrap().await, Err("sync"));
    let provider = SpanAttributes::new().with("provider", "example");
    assert!(
        start
            .start_span("request", SpanAttributes::new().with("kind", "read"), |_, _| async { Ok::<_, String>(()) })
            .is_err()
    );
    let failed = start.start_span("request", provider, |_, _| async { Err::<(), _>("async") });
    assert_eq!(failed.unwrap().await, Err("async"));

    let duplicate = create_typed_span_starter(context.clone(), &[operation_schema(), operation_schema()]);
    assert!(duplicate.is_err(), "duplicate span names across schemas are rejected");
}

#[tokio::test]
async fn noop_admits_callbacks_synchronously_and_reuses_one_inert_span() {
    let admitted = Arc::new(AtomicBool::new(false));
    let flag = admitted.clone();
    let result = NOOP_TELEMETRY_CONTEXT.start_infallible_span(SpanOptions::new("first"), move |span| {
        flag.store(true, Ordering::SeqCst);
        async move {
            let child: Arc<dyn TelemetrySpan> =
                span.start_infallible_span(SpanOptions::new("child"), |child| async move { child }).await;
            assert!(Arc::ptr_eq(&child, &span));
            42
        }
    });
    assert!(admitted.load(Ordering::SeqCst));
    assert_eq!(result.await, 42);
}

#[tokio::test]
async fn noop_preserves_errors_and_ignores_payloads() {
    let sync = NOOP_TELEMETRY_CONTEXT.start_span(SpanOptions::new("sync"), |_| async { Err::<(), _>("sync") });
    assert_eq!(sync.await, Err("sync"));
    let options = SpanOptions::new("operation").with_attributes(SpanAttributes::new().with("secret", "prompt"));
    NOOP_TELEMETRY_CONTEXT
        .start_infallible_span(options, |span| async move {
            span.add_event("event", SpanAttributes::new().with("secret", "content"));
            span.set_attributes(SpanAttributes::new().with("secret", "content"));
            span.set_status(SpanStatus::Ok);
        })
        .await;
}

#[test]
fn propagates_known_spans_and_falls_back_to_the_root_for_unknown_carriers() {
    let telemetry = InMemoryTelemetryContext::new();
    let parent = telemetry.open_span(SpanOptions::new("parent"));
    let carrier = telemetry.inject(parent.as_ref()).expect("a recorded span has a carrier");
    parent.end(SpanOutcome::Ok);
    assert_eq!(telemetry.inject(&telemetry), None);
    // A settled span still propagates; its carrier parents a new span in the same recorder.
    let child = telemetry.extract(Some(&carrier)).open_span(SpanOptions::new("resumed"));
    assert_eq!(telemetry.inject(child.as_ref()).map(|c| c["pi-memory-span"].clone()), Some("2".into()));
    let other = InMemoryTelemetryContext::new();
    other.extract(Some(&carrier)).open_span(SpanOptions::new("foreign")).end(SpanOutcome::Ok);
    let garbled = TelemetryCarrier::from([("pi-memory-scope".to_owned(), "x".to_owned())]);
    telemetry.extract(Some(&garbled)).open_span(SpanOptions::new("garbled")).end(SpanOutcome::Ok);
    child.end(SpanOutcome::Ok);

    let spans = telemetry.get_spans();
    assert_eq!((spans[1].parent_id, spans[1].parent_context.as_ref()), (Some(1), Some(&carrier)));
    assert_eq!((spans[2].parent_id, spans[2].parent_context.as_ref()), (None, None));
    let foreign = &other.get_spans()[0];
    assert_eq!((foreign.parent_id, foreign.parent_context.as_ref()), (None, Some(&carrier)));
}

#[test]
fn records_links_and_keeps_capabilities_under_a_settled_parent() {
    let telemetry = InMemoryTelemetryContext::new();
    let target = telemetry.open_span(SpanOptions::new("target"));
    let attributes = SpanAttributes::new().with("why", "steer");
    let link = SpanLink { context: target.clone(), attributes: attributes.clone() };
    let linked = telemetry.open_span(SpanOptions::new("linked").with_links(vec![link]));
    linked.add_link(SpanLink::new(Arc::new(InMemoryTelemetryContext::new())));
    linked.add_link(SpanLink::new(target.clone()));
    target.end(SpanOutcome::Ok);
    let inert = target.open_span(SpanOptions::new("late"));
    assert!(inert.metrics().is_some() && inert.propagation().is_some());
    assert_eq!(telemetry.inject(inert.as_ref()), None);
    linked.end(SpanOutcome::Ok);

    let spans = telemetry.get_spans();
    assert_eq!(spans.len(), 2);
    let links: Vec<_> = spans[1].links.iter().map(|link| (&link.context, link.attributes.len())).collect();
    assert_eq!(links, [(&spans[0].context, 1), (&spans[0].context, 0)]);
}

#[test]
fn records_measurements_and_drops_invalid_ones() {
    let telemetry = InMemoryTelemetryContext::new();
    let span = telemetry.open_span(SpanOptions::new("phase"));
    let counter = telemetry.counter("requests", InstrumentOptions::default());
    counter.add(1.0, SpanAttributes::new().with("kind", "a"));
    counter.add(-1.0, SpanAttributes::new());
    counter.add(f64::NAN, SpanAttributes::new());
    telemetry.up_down_counter("active", InstrumentOptions::default()).add(-1.0, SpanAttributes::new());
    let options = HistogramOptions { unit: Some("ms".into()), boundaries: Some(vec![1.0, 10.0]), ..Default::default() };
    telemetry.histogram("duration", options).record(5.0, SpanAttributes::new(), Some(span.as_ref()));
    let unordered = HistogramOptions { boundaries: Some(vec![10.0, 1.0]), ..Default::default() };
    telemetry.histogram("unordered", unordered).record(5.0, SpanAttributes::new(), None);

    let metrics = telemetry.get_metrics();
    let summary: Vec<_> = metrics.iter().map(|m| (m.name.as_str(), m.value)).collect();
    assert_eq!(summary, [("requests", 1.0), ("active", -1.0), ("duration", 5.0)]);
    assert_eq!(metrics[2].context, telemetry.inject(span.as_ref()));
    assert_eq!((metrics[2].unit.as_deref(), metrics[2].boundaries.as_deref()), (Some("ms"), Some(&[1.0, 10.0][..])));
}

#[test]
fn checks_typed_metric_names_kinds_and_attributes() {
    let metrics = schema(json!({
        "version": 1,
        "spans": {},
        "metrics": {
            "calls": {"kind": "counter", "description": "Calls", "unit": "1", "attributes": {
                "kind": {"type": "string", "required": true, "values": ["a", "b"], "description": "Kind"}
            }},
            "latency": {"kind": "histogram", "description": "Latency", "boundaries": [1, 2], "attributes": {}}
        }
    }));
    let telemetry = InMemoryTelemetryContext::new();
    let typed = create_typed_metrics(Arc::new(telemetry.clone()), std::slice::from_ref(&metrics)).unwrap();
    assert!(create_typed_metrics(Arc::new(telemetry.clone()), &[metrics.clone(), metrics.clone()]).is_err());
    assert!(typed.histogram("calls").is_err());
    assert!(typed.counter("missing").is_err());
    let calls = typed.counter("calls").unwrap();
    assert!(calls.add(1.0, SpanAttributes::new()).is_err());
    assert!(calls.add(1.0, SpanAttributes::new().with("kind", "c")).is_err());
    calls.add(2.0, SpanAttributes::new().with("kind", "a")).unwrap();
    typed.histogram("latency").unwrap().record(3.0, SpanAttributes::new(), None).unwrap();

    let recorded = telemetry.get_metrics();
    assert_eq!(recorded.len(), 2);
    assert_eq!((recorded[0].description.as_deref(), recorded[0].unit.as_deref()), (Some("Calls"), Some("1")));
    assert_eq!(recorded[1].boundaries.as_deref(), Some(&[1.0, 2.0][..]));
}
