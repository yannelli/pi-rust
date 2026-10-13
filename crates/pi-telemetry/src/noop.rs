//! The shared inert context used when an application provides none.

use std::sync::{Arc, LazyLock};

use crate::attributes::SpanAttributes;
use crate::span::{SpanOptions, SpanOutcome, SpanStatus, TelemetryContext, TelemetrySpan};

/// Records nothing and never inspects payloads. Every span it opens is the same inert span.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetryContext;

static NOOP_SPAN: LazyLock<Arc<dyn TelemetrySpan>> = LazyLock::new(|| Arc::new(NoopTelemetryContext));

/// Shared telemetry context used when an application does not provide one.
pub static NOOP_TELEMETRY_CONTEXT: NoopTelemetryContext = NoopTelemetryContext;

/// The no-op context as a shareable handle.
pub fn noop_telemetry_context() -> Arc<dyn TelemetryContext> {
    Arc::new(NoopTelemetryContext)
}

impl TelemetryContext for NoopTelemetryContext {
    fn open_span(&self, _options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        NOOP_SPAN.clone()
    }
}

impl TelemetrySpan for NoopTelemetryContext {
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
    fn end(&self, _outcome: SpanOutcome) {}
}
