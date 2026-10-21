//! Backend-independent stored telemetry: settled span snapshots, trace and metric queries, and the
//! [`TelemetryStore`] read contract (upstream `StoredSpan`, `TraceQuery`, `MetricQuery`, and `TelemetryStore`).

use std::error::Error;
use std::fmt;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::attributes::SpanAttributes;
use crate::metrics::TelemetryMetricKind;
use crate::span::SpanStatus;

/// An event of a stored span. `time_ms` is relative to the span start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEvent {
    pub name: String,
    pub time_ms: f64,
    pub attributes: SpanAttributes,
}

/// A link to a span of any trace; both identities are kept because a link never follows its target.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredLink {
    pub trace_id: String,
    pub span_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<SpanAttributes>,
}

/// A settled span snapshot, independent of the recording adapter. Times are epoch milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredSpan {
    pub id: String,
    pub trace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub name: String,
    pub started_ms: f64,
    pub ended_ms: f64,
    pub attributes: SpanAttributes,
    pub events: Vec<StoredEvent>,
    pub links: Vec<StoredLink>,
    pub status: SpanStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTrace {
    pub trace_id: String,
    pub spans: Vec<StoredSpan>,
}

/// Trace filters and paging. An unset field does not filter.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TraceQuery {
    /// Name of the earliest stored span, with span ID breaking timestamp ties.
    pub name: Option<String>,
    /// Scalar equalities that must all match one span, anywhere in the trace.
    pub attributes: SpanAttributes,
    /// Keep only traces with an error span.
    pub errors: bool,
    /// Inclusive bounds on the earliest stored span's start time.
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    /// Positive page size; defaults to 20.
    pub limit: Option<usize>,
    /// Opaque continuation from an earlier page; keep the same filters.
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracePage {
    pub traces: Vec<StoredTrace>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MetricQuery {
    pub name: String,
    /// Inclusive bounds on interval start time.
    pub start_min: Option<f64>,
    pub start_max: Option<f64>,
    /// Scalar equalities that must all match the series.
    pub attributes: SpanAttributes,
}

/// One metric series over one interval.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredMetric {
    pub interval_ms: f64,
    pub name: String,
    pub kind: TelemetryMetricKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub attributes: SpanAttributes,
    pub count: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

/// A failed store read, such as an unreadable cursor or a backend error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(String);

impl StoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for StoreError {}

/// Read settled traces and metrics without depending on a storage backend. A store returns the spans
/// available locally, so a remote or expired parent can be absent. Paging is not a snapshot across requests.
pub trait TelemetryStore: Send + Sync {
    /// Complete locally stored traces, newest trace start first; equal starts are ordered by trace ID.
    fn find_traces(&self, query: TraceQuery) -> BoxFuture<'_, Result<TracePage, StoreError>>;

    fn get_trace<'a>(&'a self, trace_id: &'a str) -> BoxFuture<'a, Result<Option<StoredTrace>, StoreError>>;

    fn find_metrics(&self, query: MetricQuery) -> BoxFuture<'_, Result<Vec<StoredMetric>, StoreError>>;
}
