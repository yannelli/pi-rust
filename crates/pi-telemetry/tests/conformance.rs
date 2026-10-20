//! Port of upstream `test/conformance.test.ts`.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_telemetry::testing::{TelemetryAdapterFixture, create_telemetry_adapter_conformance};
use pi_telemetry::{
    InMemoryTelemetryContext, RecordedTelemetrySpan, SpanAttributes, SpanOptions, TelemetryContext, TelemetryContextExt,
};

struct MemoryFixture(InMemoryTelemetryContext);

impl TelemetryAdapterFixture for MemoryFixture {
    fn context(&self) -> Arc<dyn TelemetryContext> {
        Arc::new(self.0.clone())
    }

    fn get_spans(&self) -> BoxFuture<'_, Vec<RecordedTelemetrySpan>> {
        Box::pin(async { self.0.get_spans() })
    }
}

#[tokio::test]
async fn in_memory_context_passes_conformance() {
    let cases = create_telemetry_adapter_conformance(Arc::new(|| {
        Box::pin(async { Box::new(MemoryFixture(InMemoryTelemetryContext::new())) as Box<dyn TelemetryAdapterFixture> })
    }));
    assert!(cases.len() >= 7);
    for case in &cases {
        if let Err(error) = case.run().await {
            panic!("{} / {}: {error}", case.group, case.name);
        }
    }
}

#[tokio::test]
async fn returns_detached_snapshots_without_exposing_mutable_recording_state() {
    let context = InMemoryTelemetryContext::new();
    let observer = context.clone();
    let open = context
        .start_infallible_span(
            SpanOptions::new("snapshot").with_attributes(SpanAttributes::new().with("tags", vec!["initial"])),
            |span| async move {
                span.add_event("event", SpanAttributes::new().with("value", 1));
                observer.get_spans().remove(0)
            },
        )
        .await;
    assert!(!open.settled);
    assert_eq!(open.end_sequence, None);

    let mut first = context.get_spans().remove(0);
    assert!(first.settled);
    assert_eq!(first.end_sequence, Some(1));
    first.attributes.set("tags", vec!["mutated"]);
    first.events[0].attributes.set("value", 2);

    let second = context.get_spans().remove(0);
    assert_eq!(second.attributes, SpanAttributes::new().with("tags", vec!["initial"]));
    assert_eq!(second.events[0].attributes, SpanAttributes::new().with("value", 1));
    assert_eq!(
        serde_json::to_value(&second).unwrap(),
        serde_json::json!({
            "id": 1, "parentId": null, "context": second.context, "parentContext": null, "links": [],
            "name": "snapshot", "attributes": {"tags": ["initial"]},
            "events": [{"name": "event", "attributes": {"value": 1.0}}],
            "status": {"status": "ok"}, "settled": true, "endSequence": 1
        })
    );
}
