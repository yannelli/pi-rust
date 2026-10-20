//! The shared inert context, metrics, and propagation used when an application provides none.

use std::sync::{Arc, LazyLock};

use crate::attributes::SpanAttributes;
use crate::metrics::{
    HistogramOptions, InstrumentOptions, TelemetryCounter, TelemetryHistogram, TelemetryMetrics, TelemetryUpDownCounter,
};
use crate::span::{
    SpanLink, SpanOptions, SpanOutcome, SpanStatus, TelemetryCarrier, TelemetryContext, TelemetryPropagation,
    TelemetrySpan,
};

/// Records nothing and never inspects payloads. Every span it opens is the same inert span.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetryContext;

/// Instruments that never inspect or retain metadata, measurements, or contexts.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetryMetrics;

/// Propagation that neither inspects nor retains its inputs.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopTelemetryPropagation;

static NOOP_SPAN: LazyLock<Arc<dyn TelemetrySpan>> = LazyLock::new(|| Arc::new(NoopTelemetryContext));

/// Shared telemetry context used when an application does not provide one.
pub static NOOP_TELEMETRY_CONTEXT: NoopTelemetryContext = NoopTelemetryContext;
pub static NOOP_TELEMETRY_METRICS: NoopTelemetryMetrics = NoopTelemetryMetrics;
pub static NOOP_TELEMETRY_PROPAGATION: NoopTelemetryPropagation = NoopTelemetryPropagation;

/// The no-op context as a shareable handle.
pub fn noop_telemetry_context() -> Arc<dyn TelemetryContext> {
    Arc::new(NoopTelemetryContext)
}

/// The shared no-op span, the telemetry of a context without one.
pub fn noop_telemetry_span() -> Arc<dyn TelemetrySpan> {
    NOOP_SPAN.clone()
}

impl TelemetryContext for NoopTelemetryContext {
    fn open_span(&self, _options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        NOOP_SPAN.clone()
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(NoopTelemetryMetrics))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(NoopTelemetryPropagation))
    }
}

impl TelemetrySpan for NoopTelemetryContext {
    fn add_link(&self, _link: SpanLink) {}
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
    fn end(&self, _outcome: SpanOutcome) {}
}

impl TelemetryCounter for NoopTelemetryMetrics {
    fn add(&self, _value: f64, _attributes: SpanAttributes) {}
}

impl TelemetryUpDownCounter for NoopTelemetryMetrics {
    fn add(&self, _value: f64, _attributes: SpanAttributes) {}
}

impl TelemetryHistogram for NoopTelemetryMetrics {
    fn record(&self, _value: f64, _attributes: SpanAttributes, _context: Option<&dyn TelemetryContext>) {}
}

impl TelemetryMetrics for NoopTelemetryMetrics {
    fn counter(&self, _name: &str, _options: InstrumentOptions) -> Arc<dyn TelemetryCounter> {
        Arc::new(NoopTelemetryMetrics)
    }

    fn up_down_counter(&self, _name: &str, _options: InstrumentOptions) -> Arc<dyn TelemetryUpDownCounter> {
        Arc::new(NoopTelemetryMetrics)
    }

    fn histogram(&self, _name: &str, _options: HistogramOptions) -> Arc<dyn TelemetryHistogram> {
        Arc::new(NoopTelemetryMetrics)
    }
}

impl TelemetryPropagation for NoopTelemetryPropagation {
    fn inject(&self, _context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        None
    }

    fn extract(&self, _carrier: Option<&TelemetryCarrier>) -> Arc<dyn TelemetryContext> {
        noop_telemetry_context()
    }
}
