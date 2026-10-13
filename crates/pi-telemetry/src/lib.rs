//! Vendor-neutral telemetry contracts and typed schema utilities for pi.
//! Port of upstream `@earendil-works/pi-telemetry`: callback-scoped spans over an explicit parent
//! context, a no-op context, an in-memory reference recorder, and adapter conformance cases.

mod attributes;
mod memory;
mod noop;
mod schema;
mod span;
pub mod testing;

pub use attributes::{AttributeValue, SpanAttributes};
pub use memory::{InMemoryTelemetryContext, RecordedTelemetryEvent, RecordedTelemetrySpan};
pub use noop::{NOOP_TELEMETRY_CONTEXT, NoopTelemetryContext, noop_telemetry_context};
pub use schema::{
    SchemaTelemetrySpan, SchemaViolation, TelemetryAttributeCardinality, TelemetryAttributeDefinition,
    TelemetryAttributeType, TelemetryEventDefinition, TelemetryParentDefinition, TelemetrySchemaDefinition,
    TelemetrySpanDefinition, TelemetryStatusDefinition, TypedSpanStarter, create_typed_span_starter,
    define_telemetry_schema,
};
pub use span::{
    SpanErrorInfo, SpanFuture, SpanOptions, SpanOutcome, SpanStatus, TelemetryContext, TelemetryContextExt,
    TelemetrySpan,
};
