//! Backend-neutral reference implementation that records spans and measurements in process memory.

use std::any::Any;
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::attributes::SpanAttributes;
use crate::metrics::{
    HistogramOptions, InstrumentOptions, TelemetryCounter, TelemetryHistogram, TelemetryMetricKind, TelemetryMetrics,
    TelemetryUpDownCounter,
};
use crate::span::{
    SpanLink, SpanOptions, SpanOutcome, SpanStatus, TelemetryCarrier, TelemetryContext, TelemetryPropagation,
    TelemetrySpan,
};

const SCOPE_KEY: &str = "pi-memory-scope";
const SPAN_KEY: &str = "pi-memory-span";

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

struct Recorded {
    span: RecordedTelemetrySpan,
    explicit_status: bool,
}

struct State {
    spans: Vec<Recorded>,
    measurements: Vec<RecordedTelemetryMeasurement>,
    next_span_id: u64,
    next_end_sequence: u64,
}

/// Records spans in start order and measurements in recording order. Create a fresh instance to isolate tests or
/// recording scopes.
#[derive(Clone)]
pub struct InMemoryTelemetryContext {
    state: Arc<Mutex<State>>,
    scope: Arc<str>,
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

fn is_scope(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(index, c)| match index {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('a'..='f').contains(&c),
        })
}

fn span_id(value: &str) -> Option<u64> {
    if value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok().filter(|id| *id < (1u64 << 53))
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
        let state = State { spans: Vec::new(), measurements: Vec::new(), next_span_id: 1, next_end_sequence: 1 };
        Self { state: Arc::new(Mutex::new(state)), scope: random_scope().into() }
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
        self.lock().measurements.clone()
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
        let carrier =
            TelemetryCarrier::from([(SCOPE_KEY.into(), self.scope.to_string()), (SPAN_KEY.into(), id.to_string())]);
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
        state.spans.push(Recorded { span, explicit_status: false });
        Arc::new(MemorySpan { root: self.clone(), index: state.spans.len() - 1, carrier })
    }

    fn instrument(&self, kind: TelemetryMetricKind, name: &str, options: HistogramOptions) -> Arc<MemoryInstrument> {
        let boundaries = options.boundaries.filter(|_| kind == TelemetryMetricKind::Histogram);
        let increasing = boundaries.as_ref().is_none_or(|boundaries| {
            boundaries.iter().all(|value| value.is_finite()) && boundaries.windows(2).all(|pair| pair[0] < pair[1])
        });
        Arc::new(MemoryInstrument {
            root: self.clone(),
            valid: increasing,
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

    /// Unrecognized carriers fall back to this recorder's root context.
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Arc<dyn TelemetryContext> {
        let parsed = carrier.and_then(|carrier| {
            let scope = carrier.get(SCOPE_KEY).filter(|scope| is_scope(scope))?;
            let id = span_id(carrier.get(SPAN_KEY)?)?;
            Some((scope.clone(), id))
        });
        let Some((scope, id)) = parsed else { return Arc::new(self.clone()) };
        let local = *scope == *self.scope && self.lock().spans.iter().any(|recorded| recorded.span.id == id);
        let carrier = TelemetryCarrier::from([(SCOPE_KEY.into(), scope), (SPAN_KEY.into(), id.to_string())]);
        Arc::new(ExtractedParent { root: self.clone(), carrier, local_id: local.then_some(id) })
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
    /// Invalid metadata, such as unordered boundaries, makes the instrument inert.
    valid: bool,
    kind: TelemetryMetricKind,
    name: String,
    description: Option<String>,
    unit: Option<String>,
    boundaries: Option<Vec<f64>>,
}

impl MemoryInstrument {
    fn measure(&self, value: f64, attributes: SpanAttributes, context: Option<&dyn TelemetryContext>) {
        if !self.valid || !value.is_finite() || (self.kind == TelemetryMetricKind::Counter && value < 0.0) {
            return;
        }
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
        self.root.lock().measurements.push(measurement);
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
        self.update(|recorded| recorded.span.events.push(RecordedTelemetryEvent { name: name.into(), attributes }));
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
        recorded.span.end_sequence = Some(sequence);
        state.next_end_sequence += 1;
    }
}
