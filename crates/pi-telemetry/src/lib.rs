//! Vendor-neutral telemetry contracts and typed schema utilities for pi.
//! Port of upstream `@earendil-works/pi-telemetry`: callback-scoped spans over an explicit parent
//! context, optional propagation and metrics capabilities, a no-op context, an in-memory reference
//! recorder that is also a [`TelemetryStore`], a trace renderer ([`render`]), and adapter conformance cases.

mod attributes;
mod memory;
mod metrics;
mod noop;
pub mod render;
mod schema;
mod span;
mod store;
pub mod testing;

pub use attributes::{AttributeValue, SpanAttributes};
pub use memory::{
    InMemoryTelemetryContext, RecordedTelemetryEvent, RecordedTelemetryLink, RecordedTelemetryMeasurement,
    RecordedTelemetrySpan, TelemetryClock,
};
pub use metrics::{
    HistogramOptions, InstrumentOptions, SchemaTelemetryCounter, SchemaTelemetryHistogram, TelemetryCounter,
    TelemetryHistogram, TelemetryMetricDefinition, TelemetryMetricKind, TelemetryMetrics, TelemetryUpDownCounter,
    TypedTelemetryMetrics, create_typed_metrics,
};
pub use noop::{
    NOOP_TELEMETRY_CONTEXT, NOOP_TELEMETRY_METRICS, NOOP_TELEMETRY_PROPAGATION, NoopTelemetryContext,
    NoopTelemetryMetrics, NoopTelemetryPropagation, noop_telemetry_context, noop_telemetry_span,
};
pub use schema::{
    SchemaTelemetrySpan, SchemaViolation, TelemetryAttributeCardinality, TelemetryAttributeDefinition,
    TelemetryAttributeType, TelemetryEventDefinition, TelemetryParentDefinition, TelemetrySchemaDefinition,
    TelemetrySpanDefinition, TelemetryStatusDefinition, TypedSpanStarter, create_typed_span_starter,
    define_telemetry_schema,
};
pub use span::{
    SpanErrorInfo, SpanFuture, SpanLink, SpanOptions, SpanOutcome, SpanStatus, TelemetryCarrier, TelemetryContext,
    TelemetryContextExt, TelemetryPropagation, TelemetrySpan,
};
pub use store::{
    MetricQuery, StoreError, StoredEvent, StoredLink, StoredMetric, StoredSpan, StoredTrace, TelemetryStore, TracePage,
    TraceQuery,
};
