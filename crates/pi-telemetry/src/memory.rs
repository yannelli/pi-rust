//! Backend-neutral reference implementation that records spans and measurements in process memory.

use std::any::Any;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

mod store;

use crate::attributes::SpanAttributes;
use crate::metrics::{
    HistogramOptions, InstrumentOptions, TelemetryCounter, TelemetryHistogram, TelemetryMetricKind, TelemetryMetrics,
    TelemetryUpDownCounter,
};
use crate::span::{
    SpanLink, SpanOptions, SpanOutcome, SpanStatus, TelemetryCarrier, TelemetryContext, TelemetryPropagation,
    TelemetrySpan,
};

pub(crate) const SCOPE_KEY: &str = "pi-memory-scope";
pub(crate) const SPAN_KEY: &str = "pi-memory-span";
pub(crate) const TRACE_KEY: &str = "pi-memory-trace";
/// Measurements are grouped into ten-second intervals for store queries.
const INTERVAL_MS: f64 = 10_000.0;

type MillisFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Time sources of a recorder: wall-clock epoch milliseconds for span and interval starts, and a monotonic
/// millisecond counter for durations and event offsets.
#[derive(Clone)]
pub struct TelemetryClock {
    wall_ms: MillisFn,
    monotonic_ms: MillisFn,
}

impl TelemetryClock {
    pub fn new(
        wall_ms: impl Fn() -> f64 + Send + Sync + 'static,
        monotonic_ms: impl Fn() -> f64 + Send + Sync + 'static,
    ) -> Self {
        Self { wall_ms: Arc::new(wall_ms), monotonic_ms: Arc::new(monotonic_ms) }
    }
}

impl Default for TelemetryClock {
    fn default() -> Self {
        static START: LazyLock<Instant> = LazyLock::new(Instant::now);
        let wall = || SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |elapsed| elapsed.as_millis() as f64);
        Self::new(wall, || START.elapsed().as_secs_f64() * 1000.0)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryEvent {
    pub name: String,
    pub attributes: SpanAttributes,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryLink {
    pub context: TelemetryCarrier,
    pub attributes: SpanAttributes,
}

/// A detached snapshot of one recorded span.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordedTelemetrySpan {
    pub id: u64,
    /// Local parent ID; `None` for roots and parents owned by another recorder.
    pub parent_id: Option<u64>,
    pub context: TelemetryCarrier,
    pub parent_context: Option<TelemetryCarrier>,
    pub links: Vec<RecordedTelemetryLink>,
    pub name: String,
    pub attributes: SpanAttributes,
    pub events: Vec<RecordedTelemetryEvent>,
    pub status: SpanStatus,
    pub settled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_sequence: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedTelemetryMeasurement {
    pub kind: TelemetryMetricKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundaries: Option<Vec<f64>>,
    pub value: f64,
    pub attributes: SpanAttributes,
    pub context: Option<TelemetryCarrier>,
}

/// A recorded span with the timing that its normalized snapshot leaves out.
struct Recorded {
    span: RecordedTelemetrySpan,
    explicit_status: bool,
    started_ms: f64,
    started_tick: f64,
    ended_ms: Option<f64>,
    /// Offsets from the span start, one per recorded event.
    event_offsets: Vec<f64>,
}

struct State {
    spans: Vec<Recorded>,
    /// Each measurement with the start of its ten-second interval.
    measurements: Vec<(f64, RecordedTelemetryMeasurement)>,
    next_span_id: u64,
    next_end_sequence: u64,
}

/// Records spans in start order and measurements in recording order. Create a fresh instance to isolate tests or
/// recording scopes.
#[derive(Clone)]
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<State>>,
    scope: Arc<str>,
    clock: TelemetryClock,
}

impl Default for InMemoryTelemetryContext {
    fn default() -> Self {
        Self::new()
    }
}

/// A random UUID-shaped scope, so carriers from another recorder are told apart.
fn random_scope() -> String {
    let hex = format!("{:016x}{:016x}", RandomState::new().hash_one(1u8), RandomState::new().hash_one(2u8));
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// The carrier of a span or extracted parent of any in-memory recorder, including settled spans.
fn carrier_of(context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
    let any = context.as_any()?;
    if let Some(span) = any.downcast_ref::<MemorySpan>() {
        return Some(span.carrier.clone());
    }
    any.downcast_ref::<ExtractedParent>().map(|parent| parent.carrier.clone())
}

fn record_links(links: &[SpanLink]) -> Vec<RecordedTelemetryLink> {
    let recorded = links.iter().filter_map(|link| {
        let context = carrier_of(link.context.as_ref())?;
        Some(RecordedTelemetryLink { context, attributes: link.attributes.clone() })
    });
    recorded.collect()
}

enum Parent {
    Root,
    Span(usize),
    Extracted { carrier: TelemetryCarrier, local_id: Option<u64> },
}

impl InMemoryTelemetryContext {
    pub fn new() -> Self {
        Self::with_clock(TelemetryClock::default())
    }

    /// A recorder that reads time from `clock`, so tests can fix span and interval times.
    pub fn with_clock(clock: TelemetryClock) -> Self {
        let state = State { spans: Vec::new(), measurements: Vec::new(), next_span_id: 1, next_end_sequence: 1 };
        Self { state: Arc::new(Mutex::new(state)), scope: random_scope().into(), clock }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Detached snapshots in span-start order.
    pub fn get_spans(&self) -> Vec<RecordedTelemetrySpan> {
        self.lock().spans.iter().map(|recorded| recorded.span.clone()).collect()
    }

    /// Detached measurements in recording order, without aggregation.
    pub fn get_metrics(&self) -> Vec<RecordedTelemetryMeasurement> {
        self.lock().measurements.iter().map(|(_, measurement)| measurement.clone()).collect()
    }

    fn open(&self, parent: Parent, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        let links = record_links(&options.links);
        let mut state = self.lock();
        let (parent_id, parent_context) = match parent {
            Parent::Root => (None, None),
            Parent::Span(index) if state.spans[index].span.settled => return Arc::new(InertSpan(self.clone())),
            Parent::Span(index) => {
                let span = &state.spans[index].span;
                (Some(span.id), Some(span.context.clone()))
            }
            Parent::Extracted { carrier, local_id } => (local_id, Some(carrier)),
        };
        let id = state.next_span_id;
        state.next_span_id += 1;
        let trace_id = parent_context
            .as_ref()
            .and_then(|context| context.get(TRACE_KEY).cloned())
            .unwrap_or_else(|| format!("{}:{id}", self.scope));
        let carrier = TelemetryCarrier::from([
            (SCOPE_KEY.into(), self.scope.to_string()),
            (SPAN_KEY.into(), id.to_string()),
            (TRACE_KEY.into(), trace_id),
        ]);
        let span = RecordedTelemetrySpan {
            id,
            parent_id,
            context: carrier.clone(),
            parent_context,
            links,
            name: options.name,
            attributes: options.attributes,
            events: Vec::new(),
            status: SpanStatus::Ok,
            settled: false,
            end_sequence: None,
        };
        let recorded = Recorded {
            span,
            explicit_status: false,
            started_ms: (self.clock.wall_ms)(),
            started_tick: (self.clock.monotonic_ms)(),
            ended_ms: None,
            event_offsets: Vec::new(),
        };
        state.spans.push(recorded);
        Arc::new(MemorySpan { root: self.clone(), index: state.spans.len() - 1, carrier })
    }

    fn instrument(&self, kind: TelemetryMetricKind, name: &str, options: HistogramOptions) -> Arc<MemoryInstrument> {
        let boundaries = options.boundaries.filter(|_| kind == TelemetryMetricKind::Histogram);
        Arc::new(MemoryInstrument {
            root: self.clone(),
            kind,
            name: name.into(),
            description: options.description,
            unit: options.unit,
            boundaries,
        })
    }
}

impl TelemetryContext for InMemoryTelemetryContext {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.open(Parent::Root, options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(self.clone()))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(self.clone()))
    }
}

impl TelemetryPropagation for InMemoryTelemetryContext {
    fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        carrier_of(context)
    }

    /// A carrier without all three identities falls back to this recorder's root context. The identities are
    /// opaque strings, so any format is accepted.
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Arc<dyn TelemetryContext> {
        let identities = carrier
            .and_then(|carrier| Some([carrier.get(SCOPE_KEY)?, carrier.get(SPAN_KEY)?, carrier.get(TRACE_KEY)?]));
        let Some([scope, span, trace]) = identities else { return Arc::new(self.clone()) };
        let local_id = if **scope == *self.scope { span.parse().ok() } else { None };
        let carrier = TelemetryCarrier::from([
            (SCOPE_KEY.into(), scope.clone()),
            (SPAN_KEY.into(), span.clone()),
            (TRACE_KEY.into(), trace.clone()),
        ]);
        Arc::new(ExtractedParent { root: self.clone(), carrier, local_id })
    }
}

impl TelemetryMetrics for InMemoryTelemetryContext {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryCounter> {
        self.instrument(TelemetryMetricKind::Counter, name, histogram_options(options))
    }

    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryUpDownCounter> {
        self.instrument(TelemetryMetricKind::UpDownCounter, name, histogram_options(options))
    }

    fn histogram(&self, name: &str, options: HistogramOptions) -> Arc<dyn TelemetryHistogram> {
        self.instrument(TelemetryMetricKind::Histogram, name, options)
    }
}

fn histogram_options(options: InstrumentOptions) -> HistogramOptions {
    HistogramOptions { description: options.description, unit: options.unit, boundaries: None }
}

struct MemoryInstrument {
    root: InMemoryTelemetryContext,
    kind: TelemetryMetricKind,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    boundaries: Option<Vec<f64>>,
}

impl MemoryInstrument {
    fn measure(&self, value: f64, attributes: SpanAttributes, context: Option<&dyn TelemetryContext>) {
        let measurement = RecordedTelemetryMeasurement {
            kind: self.kind,
            name: self.name.clone(),
            description: self.description.clone(),
            unit: self.unit.clone(),
            boundaries: self.boundaries.clone(),
            value,
            attributes,
            context: context.and_then(carrier_of),
        };
        let interval = ((self.root.clock.wall_ms)() / INTERVAL_MS).floor() * INTERVAL_MS;
        self.root.lock().measurements.push((interval, measurement));
    }
}

impl TelemetryCounter for MemoryInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.measure(value, attributes, None);
    }
}

impl TelemetryUpDownCounter for MemoryInstrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.measure(value, attributes, None);
    }
}

impl TelemetryHistogram for MemoryInstrument {
    fn record(&self, value: f64, attributes: SpanAttributes, context: Option<&dyn TelemetryContext>) {
        self.measure(value, attributes, context);
    }
}

/// A parent rebuilt from a carrier; spans under it record the carrier as their parent context.
struct ExtractedParent {
    root: InMemoryTelemetryContext,
    carrier: TelemetryCarrier,
    local_id: Option<u64>,
}

impl TelemetryContext for ExtractedParent {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        let parent = Parent::Extracted { carrier: self.carrier.clone(), local_id: self.local_id };
        self.root.open(parent, options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        self.root.metrics()
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        self.root.propagation()
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

/// A span opened under a settled span: it records nothing but keeps the recorder's capabilities.
struct InertSpan(InMemoryTelemetryContext);

impl TelemetryContext for InertSpan {
    fn open_span(&self, _options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        Arc::new(InertSpan(self.0.clone()))
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        self.0.metrics()
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        self.0.propagation()
    }
}

impl TelemetrySpan for InertSpan {
    fn add_link(&self, _link: SpanLink) {}
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
    fn end(&self, _outcome: SpanOutcome) {}
}

struct MemorySpan {
    root: InMemoryTelemetryContext,
    index: usize,
    carrier: TelemetryCarrier,
}

impl MemorySpan {
    /// Apply `update` while the span is open; calls after settlement are inert.
    fn update(&self, update: impl FnOnce(&mut Recorded)) {
        let mut state = self.root.lock();
        let recorded = &mut state.spans[self.index];
        if !recorded.span.settled {
            update(recorded);
        }
    }
}

impl TelemetryContext for MemorySpan {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.root.open(Parent::Span(self.index), options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        self.root.metrics()
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        self.root.propagation()
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl TelemetrySpan for MemorySpan {
    fn add_link(&self, link: SpanLink) {
        let Some(context) = carrier_of(link.context.as_ref()) else { return };
        let link = RecordedTelemetryLink { context, attributes: link.attributes };
        self.update(|recorded| recorded.span.links.push(link));
    }

    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        let tick = (self.root.clock.monotonic_ms)();
        self.update(|recorded| {
            recorded.event_offsets.push(tick - recorded.started_tick);
            recorded.span.events.push(RecordedTelemetryEvent { name: name.into(), attributes });
        });
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        self.update(|recorded| recorded.span.attributes.merge(&attributes));
    }

    fn set_status(&self, status: SpanStatus) {
        self.update(|recorded| {
            recorded.span.status = status;
            recorded.explicit_status = true;
        });
    }

    fn end(&self, outcome: SpanOutcome) {
        let tick = (self.root.clock.monotonic_ms)();
        let mut state = self.root.lock();
        let sequence = state.next_end_sequence;
        let recorded = &mut state.spans[self.index];
        if recorded.span.settled {
            return;
        }
        if let SpanOutcome::Failed(error) = outcome
            && !recorded.explicit_status
        {
            recorded.span.status = SpanStatus::Error { error };
        }
        recorded.span.settled = true;
        recorded.ended_ms = Some(recorded.started_ms + tick - recorded.started_tick);
        recorded.span.end_sequence = Some(sequence);
        state.next_end_sequence += 1;
    }
}
