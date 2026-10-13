//! Port of upstream `test/telemetry.test.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pi_telemetry::{
    InMemoryTelemetryContext, NOOP_TELEMETRY_CONTEXT, SpanAttributes, SpanOptions, SpanStatus, TelemetryContextExt,
    TelemetrySchemaDefinition, TelemetrySpan, create_typed_span_starter, define_telemetry_schema,
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
