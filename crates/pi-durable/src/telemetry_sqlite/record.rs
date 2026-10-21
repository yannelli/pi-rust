//! Span, propagation, and metric recording of the SQLite recorder. Recording never runs SQL: settled spans and
//! metric aggregates wait in memory for the next flush.

use std::any::Any;
use std::sync::{Arc, Mutex, MutexGuard};

use pi_telemetry::{
    HistogramOptions, InstrumentOptions, SpanAttributes, SpanLink, SpanOptions, SpanOutcome, SpanStatus,
    TelemetryCarrier, TelemetryContext, TelemetryCounter, TelemetryHistogram, TelemetryMetricKind, TelemetryMetrics,
    TelemetryPropagation, TelemetrySpan, TelemetryUpDownCounter,
};

use super::encode::{self, Event, Link};
use super::{Metric, Recorder, SpanRow, SqliteTelemetry, interval_of};
use crate::storage::sqlite::SqliteValue;

const SPAN_KEY: &str = "pi-sqlite-span";
const TRACE_KEY: &str = "pi-sqlite-trace";

#[derive(Clone, Debug)]
struct Identity {
    id: String,
    trace_id: String,
}

/// The identity of a span or extracted parent of any SQLite recorder, including settled spans.
fn identity_of(context: &dyn TelemetryContext) -> Option<Identity> {
    let any = context.as_any()?;
    if let Some(span) = any.downcast_ref::<SqliteSpan>() {
        return Some(span.identity.clone());
    }
    any.downcast_ref::<ExtractedParent>().map(|parent| parent.identity.clone())
}

fn copy_link(link: &SpanLink) -> Option<Link> {
    let identity = identity_of(link.context.as_ref())?;
    Some(Link { trace_id: identity.trace_id, span_id: identity.id, attributes: link.attributes.clone() })
}

impl Recorder {
    fn telemetry(self: &Arc<Self>) -> SqliteTelemetry {
        SqliteTelemetry { recorder: self.clone() }
    }

    fn open(self: &Arc<Self>, parent: Option<&Identity>, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        let links: Vec<Link> = options.links.iter().filter_map(copy_link).collect();
        let (started_ms, started_tick) = (self.clock.wall_ms(), self.clock.monotonic_ms());
        let id = {
            let mut state = self.lock();
            if state.stopped {
                return Arc::new(InertSpan(self.clone()));
            }
            state.sequence += 1;
            format!("{}:{}", self.scope, state.sequence)
        };
        let trace_id = parent.map_or_else(|| id.clone(), |parent| parent.trace_id.clone());
        let state = SpanState {
            attributes: options.attributes,
            events: Vec::new(),
            links,
            status: SpanStatus::Ok,
            explicit: false,
            settled: false,
        };
        Arc::new(SqliteSpan {
            recorder: self.clone(),
            identity: Identity { id, trace_id },
            parent_id: parent.map(|parent| parent.id.clone()),
            name: options.name,
            started_ms,
            started_tick,
            state: Mutex::new(state),
        })
    }

    /// Queue a settled span unless recording stopped; a full queue drops and counts it.
    fn settle(&self, row: SpanRow) {
        let mut state = self.lock();
        if state.stopped {
            return;
        }
        if state.spans.len() + state.in_flight_spans >= self.capacity {
            state.dropped_spans += 1;
        } else {
            state.spans.push(row);
        }
    }

    fn measure(
        &self,
        kind: TelemetryMetricKind,
        name: &str,
        unit: Option<&str>,
        value: f64,
        attributes: &SpanAttributes,
    ) {
        if self.is_stopped() {
            return;
        }
        let encoded = encode::sorted_attributes(attributes);
        let key = (interval_of(self.clock.wall_ms()), encode::series_key(name, kind, unit, &encoded));
        let mut guard = self.lock();
        let state = &mut *guard;
        if state.stopped {
            return;
        }
        if let Some(&index) = state.pending.get(&key) {
            let metric = &mut state.measurements[index];
            metric.count += 1;
            metric.sum += value;
            metric.min = metric.min.min(value);
            metric.max = metric.max.max(value);
            return;
        }
        if state.measurements.len() + state.in_flight_measurements >= self.capacity {
            state.dropped_measurements += 1;
            return;
        }
        let (interval, series) = key.clone();
        state.pending.insert(key, state.measurements.len());
        state.measurements.push(Metric {
            interval,
            key: series,
            name: name.to_owned(),
            kind,
            unit: unit.map(String::from),
            attributes: encoded,
            count: 1,
            sum: value,
            min: value,
            max: value,
        });
    }
}

struct SpanState {
    attributes: SpanAttributes,
    events: Vec<Event>,
    links: Vec<Link>,
    status: SpanStatus,
    explicit: bool,
    settled: bool,
}

struct SqliteSpan {
    recorder: Arc<Recorder>,
    identity: Identity,
    parent_id: Option<String>,
    name: String,
    started_ms: f64,
    started_tick: f64,
    state: Mutex<SpanState>,
}

impl SqliteSpan {
    fn lock(&self) -> MutexGuard<'_, SpanState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Apply `update` while both the span and its recorder are open; later calls are inert.
    fn record(&self, update: impl FnOnce(&mut SpanState)) {
        if self.recorder.is_stopped() {
            return;
        }
        let mut state = self.lock();
        if !state.settled {
            update(&mut state);
        }
    }
}

impl TelemetryContext for SqliteSpan {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        if self.lock().settled {
            return Arc::new(InertSpan(self.recorder.clone()));
        }
        self.recorder.open(Some(&self.identity), options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(self.recorder.telemetry()))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(self.recorder.telemetry()))
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

impl TelemetrySpan for SqliteSpan {
    fn add_link(&self, link: SpanLink) {
        if let Some(link) = copy_link(&link) {
            self.record(|state| state.links.push(link));
        }
    }

    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        let time_ms = self.recorder.clock.monotonic_ms() - self.started_tick;
        self.record(|state| state.events.push(Event { name: name.to_owned(), time_ms, attributes }));
    }

    fn set_attributes(&self, attributes: SpanAttributes) {
        self.record(|state| state.attributes.merge(&attributes));
    }

    fn set_status(&self, status: SpanStatus) {
        self.record(|state| {
            state.status = status;
            state.explicit = true;
        });
    }

    fn end(&self, outcome: SpanOutcome) {
        let ended_ms = self.started_ms + self.recorder.clock.monotonic_ms() - self.started_tick;
        let row = {
            let mut state = self.lock();
            if state.settled {
                return;
            }
            state.settled = true;
            if let SpanOutcome::Failed(error) = outcome
                && !state.explicit
            {
                state.status = SpanStatus::Error { error };
            }
            [
                SqliteValue::Text(self.identity.id.clone()),
                SqliteValue::Text(self.identity.trace_id.clone()),
                SqliteValue::from(self.parent_id.clone()),
                SqliteValue::Text(self.name.clone()),
                SqliteValue::Real(self.started_ms),
                SqliteValue::Real(ended_ms),
                SqliteValue::Text(encode::attributes(&state.attributes)),
                SqliteValue::Text(encode::events(&state.events)),
                SqliteValue::Text(encode::links(&state.links)),
                SqliteValue::Text(encode::status(&state.status)),
            ]
        };
        self.recorder.settle(row);
    }
}

/// A parent rebuilt from a carrier; spans under it continue the carrier's trace.
struct ExtractedParent {
    recorder: Arc<Recorder>,
    identity: Identity,
}

impl TelemetryContext for ExtractedParent {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.recorder.open(Some(&self.identity), options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(self.recorder.telemetry()))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(self.recorder.telemetry()))
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

/// A span opened after close or under a settled span: it records nothing but keeps the recorder's capabilities.
struct InertSpan(Arc<Recorder>);

impl TelemetryContext for InertSpan {
    fn open_span(&self, _options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        Arc::new(InertSpan(self.0.clone()))
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(self.0.telemetry()))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(self.0.telemetry()))
    }
}

impl TelemetrySpan for InertSpan {
    fn add_link(&self, _link: SpanLink) {}
    fn add_event(&self, _name: &str, _attributes: SpanAttributes) {}
    fn set_attributes(&self, _attributes: SpanAttributes) {}
    fn set_status(&self, _status: SpanStatus) {}
    fn end(&self, _outcome: SpanOutcome) {}
}

impl TelemetryContext for SqliteTelemetry {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.recorder.open(None, options)
    }

    fn metrics(&self) -> Option<Arc<dyn TelemetryMetrics>> {
        Some(Arc::new(self.clone()))
    }

    fn propagation(&self) -> Option<Arc<dyn TelemetryPropagation>> {
        Some(Arc::new(self.clone()))
    }
}

impl TelemetryPropagation for SqliteTelemetry {
    fn inject(&self, context: &dyn TelemetryContext) -> Option<TelemetryCarrier> {
        let identity = identity_of(context)?;
        Some(TelemetryCarrier::from([(SPAN_KEY.into(), identity.id), (TRACE_KEY.into(), identity.trace_id)]))
    }

    /// A carrier without both identities falls back to the recorder's root context. Identities are opaque strings.
    fn extract(&self, carrier: Option<&TelemetryCarrier>) -> Arc<dyn TelemetryContext> {
        let identity = carrier.and_then(|carrier| {
            Some(Identity { id: carrier.get(SPAN_KEY)?.clone(), trace_id: carrier.get(TRACE_KEY)?.clone() })
        });
        match identity {
            Some(identity) => Arc::new(ExtractedParent { recorder: self.recorder.clone(), identity }),
            None => Arc::new(self.clone()),
        }
    }
}

struct Instrument {
    recorder: Arc<Recorder>,
    kind: TelemetryMetricKind,
    name: String,
    unit: Option<String>,
}

impl Instrument {
    fn measure(&self, value: f64, attributes: &SpanAttributes) {
        self.recorder.measure(self.kind, &self.name, self.unit.as_deref(), value, attributes);
    }
}

impl TelemetryCounter for Instrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.measure(value, &attributes);
    }
}

impl TelemetryUpDownCounter for Instrument {
    fn add(&self, value: f64, attributes: SpanAttributes) {
        self.measure(value, &attributes);
    }
}

impl TelemetryHistogram for Instrument {
    fn record(&self, value: f64, attributes: SpanAttributes, _context: Option<&dyn TelemetryContext>) {
        self.measure(value, &attributes);
    }
}

impl SqliteTelemetry {
    /// Descriptions and histogram boundaries are not stored; kind, name, and unit separate series.
    fn instrument(&self, kind: TelemetryMetricKind, name: &str, unit: Option<String>) -> Arc<Instrument> {
        Arc::new(Instrument { recorder: self.recorder.clone(), kind, name: name.to_owned(), unit })
    }
}

impl TelemetryMetrics for SqliteTelemetry {
    fn counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryCounter> {
        self.instrument(TelemetryMetricKind::Counter, name, options.unit)
    }

    fn up_down_counter(&self, name: &str, options: InstrumentOptions) -> Arc<dyn TelemetryUpDownCounter> {
        self.instrument(TelemetryMetricKind::UpDownCounter, name, options.unit)
    }

    fn histogram(&self, name: &str, options: HistogramOptions) -> Arc<dyn TelemetryHistogram> {
        self.instrument(TelemetryMetricKind::Histogram, name, options.unit)
    }
}
