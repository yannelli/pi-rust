//! The in-memory recorder as a [`TelemetryStore`] over its settled spans and raw measurements.

use std::cmp::Ordering;
use std::collections::HashMap;

use futures::future::{BoxFuture, ready};

use super::{InMemoryTelemetryContext, SCOPE_KEY, SPAN_KEY, TRACE_KEY};
use crate::attributes::SpanAttributes;
use crate::span::TelemetryCarrier;
use crate::store::{
    MetricQuery, StoreError, StoredEvent, StoredLink, StoredMetric, StoredSpan, StoredTrace, TelemetryStore, TracePage,
    TraceQuery,
};

const DEFAULT_LIMIT: usize = 20;

/// The global ID of the span a carrier names: `<recorder scope>:<span counter>`.
fn carrier_id(carrier: &TelemetryCarrier) -> String {
    let part = |key: &str| carrier.get(key).map_or("", String::as_str);
    format!("{}:{}", part(SCOPE_KEY), part(SPAN_KEY))
}

/// Every requested equality matches; scalar types never match each other.
fn matches(query: &SpanAttributes, attributes: &SpanAttributes) -> bool {
    query.iter().all(|(key, value)| attributes.get(key) == Some(value))
}

impl InMemoryTelemetryContext {
    /// Settled spans as store snapshots, ordered by start time and then ID.
    fn stored_spans(&self) -> Vec<StoredSpan> {
        let state = self.lock();
        let settled = state.spans.iter().filter(|recorded| recorded.span.settled);
        let mut spans: Vec<StoredSpan> = settled
            .map(|recorded| {
                let span = &recorded.span;
                let offsets = recorded.event_offsets.iter();
                let events = span.events.iter().zip(offsets).map(|(event, offset)| StoredEvent {
                    name: event.name.clone(),
                    time_ms: *offset,
                    attributes: event.attributes.clone(),
                });
                let links = span.links.iter().map(|link| StoredLink {
                    trace_id: link.context.get(TRACE_KEY).cloned().unwrap_or_default(),
                    span_id: carrier_id(&link.context),
                    attributes: Some(link.attributes.clone()),
                });
                StoredSpan {
                    id: carrier_id(&span.context),
                    trace_id: span.context.get(TRACE_KEY).cloned().unwrap_or_default(),
                    parent_id: span.parent_context.as_ref().map(carrier_id),
                    name: span.name.clone(),
                    started_ms: recorded.started_ms,
                    ended_ms: recorded.ended_ms.unwrap_or(recorded.started_ms),
                    attributes: span.attributes.clone(),
                    events: events.collect(),
                    links: links.collect(),
                    status: span.status.clone(),
                }
            })
            .collect();
        spans.sort_by(|left, right| left.started_ms.total_cmp(&right.started_ms).then_with(|| left.id.cmp(&right.id)));
        spans
    }

    fn trace(&self, trace_id: &str) -> Option<StoredTrace> {
        let spans: Vec<StoredSpan> = self.stored_spans().into_iter().filter(|span| span.trace_id == trace_id).collect();
        (!spans.is_empty()).then(|| StoredTrace { trace_id: trace_id.to_owned(), spans })
    }

    fn traces(&self, query: &TraceQuery) -> Result<TracePage, StoreError> {
        let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
        if limit == 0 {
            return Err(StoreError::new("Trace query limit must be positive"));
        }
        let after: Option<(f64, String)> = match &query.cursor {
            Some(cursor) => Some(
                serde_json::from_str(cursor)
                    .map_err(|error| StoreError::new(format!("Invalid trace cursor: {error}")))?,
            ),
            None => None,
        };
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, Vec<StoredSpan>> = HashMap::new();
        for span in self.stored_spans() {
            if !grouped.contains_key(&span.trace_id) {
                order.push(span.trace_id.clone());
            }
            grouped.entry(span.trace_id.clone()).or_default().push(span);
        }
        let mut matched: Vec<StoredTrace> = order
            .into_iter()
            .filter_map(|trace_id| grouped.remove(&trace_id).map(|spans| StoredTrace { trace_id, spans }))
            .filter(|trace| {
                let first = &trace.spans[0];
                query.name.as_ref().is_none_or(|name| *name == first.name)
                    && query.start_min.is_none_or(|min| first.started_ms >= min)
                    && query.start_max.is_none_or(|max| first.started_ms <= max)
                    && after.as_ref().is_none_or(|(started, id)| {
                        first.started_ms < *started || (first.started_ms == *started && trace.trace_id > *id)
                    })
                    && (!query.errors || trace.spans.iter().any(|span| span.status.is_error()))
                    && trace.spans.iter().any(|span| matches(&query.attributes, &span.attributes))
            })
            .collect();
        matched.sort_by(|left, right| {
            let (a, b) = (&left.spans[0], &right.spans[0]);
            let newest_first = b.started_ms.total_cmp(&a.started_ms);
            if newest_first == Ordering::Equal { left.trace_id.cmp(&right.trace_id) } else { newest_first }
        });
        let more = matched.len() > limit;
        matched.truncate(limit);
        let cursor = match matched.last() {
            Some(last) if more => serde_json::to_string(&(last.spans[0].started_ms, &last.trace_id)).ok(),
            _ => None,
        };
        Ok(TracePage { traces: matched, cursor })
    }

    /// Raw measurements as count-one rows in recording order.
    fn metrics(&self, query: &MetricQuery) -> Vec<StoredMetric> {
        let state = self.lock();
        let rows = state.measurements.iter().filter(|(interval, measurement)| {
            measurement.name == query.name
                && query.start_min.is_none_or(|min| *interval >= min)
                && query.start_max.is_none_or(|max| *interval <= max)
                && matches(&query.attributes, &measurement.attributes)
        });
        let stored = rows.map(|(interval, measurement)| StoredMetric {
            interval_ms: *interval,
            name: measurement.name.clone(),
            kind: measurement.kind,
            unit: measurement.unit.clone(),
            attributes: measurement.attributes.clone(),
            count: 1,
            sum: measurement.value,
            min: measurement.value,
            max: measurement.value,
        });
        stored.collect()
    }
}

impl TelemetryStore for InMemoryTelemetryContext {
    fn find_traces(&self, query: TraceQuery) -> BoxFuture<'_, Result<TracePage, StoreError>> {
        Box::pin(ready(self.traces(&query)))
    }

    fn get_trace<'a>(&'a self, trace_id: &'a str) -> BoxFuture<'a, Result<Option<StoredTrace>, StoreError>> {
        Box::pin(ready(Ok(self.trace(trace_id))))
    }

    fn find_metrics(&self, query: MetricQuery) -> BoxFuture<'_, Result<Vec<StoredMetric>, StoreError>> {
        Box::pin(ready(Ok(self.metrics(&query))))
    }
}
