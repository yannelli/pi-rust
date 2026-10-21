//! Column encodings of the telemetry tables. Text is written byte-identical to upstream `JSON.stringify`, so metric
//! series keys written here and by upstream name the same series.

use std::cmp::Ordering;

use pi_chord::{Json, JsonMap};
use pi_telemetry::{
    AttributeValue, SpanAttributes, SpanStatus, StoreError, StoredEvent, StoredLink, TelemetryMetricKind,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// A link as stored: the target's identities and the link attributes.
#[derive(Clone, Debug)]
pub(crate) struct Link {
    pub trace_id: String,
    pub span_id: String,
    pub attributes: SpanAttributes,
}

/// An event as stored, with its offset from the span start.
#[derive(Clone, Debug)]
pub(crate) struct Event {
    pub name: String,
    pub time_ms: f64,
    pub attributes: SpanAttributes,
}

fn value(value: &AttributeValue) -> Json {
    match value {
        AttributeValue::String(text) => Json::from(text.as_str()),
        AttributeValue::Number(number) => Json::from(*number),
        AttributeValue::Boolean(flag) => Json::Bool(*flag),
        AttributeValue::StringArray(items) => Json::array(items.iter().map(|item| Json::from(item.as_str())).collect()),
        AttributeValue::NumberArray(items) => Json::array(items.iter().map(|item| Json::from(*item)).collect()),
        AttributeValue::BooleanArray(items) => Json::array(items.iter().map(|item| Json::Bool(*item)).collect()),
    }
}

/// One attribute value as JSON text.
pub(crate) fn value_text(item: &AttributeValue) -> String {
    value(item).to_string()
}

fn object<'a>(entries: impl IntoIterator<Item = (&'a String, &'a AttributeValue)>) -> Json {
    let mut map = JsonMap::new();
    for (key, item) in entries {
        map.insert(key.as_str(), value(item));
    }
    Json::object(map)
}

fn attributes_json(attributes: &SpanAttributes) -> Json {
    object(attributes.iter())
}

/// Span attributes as JSON text.
pub(crate) fn attributes(attributes: &SpanAttributes) -> String {
    attributes_json(attributes).to_string()
}

/// Metric attributes with keys in JavaScript `sort()` order (UTF-16 code units), the canonical series encoding.
pub(crate) fn sorted_attributes(attributes: &SpanAttributes) -> String {
    let mut entries: Vec<_> = attributes.iter().collect();
    let utf16 = |left: &str, right: &str| -> Ordering { left.encode_utf16().cmp(right.encode_utf16()) };
    entries.sort_by(|(left, _), (right, _)| utf16(left, right));
    object(entries).to_string()
}

pub(crate) fn kind(kind: TelemetryMetricKind) -> &'static str {
    match kind {
        TelemetryMetricKind::Counter => "counter",
        TelemetryMetricKind::UpDownCounter => "upDownCounter",
        TelemetryMetricKind::Histogram => "histogram",
    }
}

/// `JSON.stringify([name, kind, unit, encoded])`: the series identity within one interval.
pub(crate) fn series_key(name: &str, metric: TelemetryMetricKind, unit: Option<&str>, encoded: &str) -> String {
    Json::array(vec![Json::from(name), Json::from(kind(metric)), Json::from(unit), Json::from(encoded)]).to_string()
}

pub(crate) fn events(events: &[Event]) -> String {
    let items = events.iter().map(|event| {
        let mut map = JsonMap::new();
        map.insert("name", event.name.as_str());
        map.insert("time_ms", event.time_ms);
        map.insert("attributes", attributes_json(&event.attributes));
        Json::object(map)
    });
    Json::array(items.collect()).to_string()
}

pub(crate) fn links(links: &[Link]) -> String {
    let items = links.iter().map(|link| {
        let mut map = JsonMap::new();
        map.insert("trace_id", link.trace_id.as_str());
        map.insert("span_id", link.span_id.as_str());
        map.insert("attributes", attributes_json(&link.attributes));
        Json::object(map)
    });
    Json::array(items.collect()).to_string()
}

pub(crate) fn status(status: &SpanStatus) -> String {
    let mut map = JsonMap::new();
    match status {
        SpanStatus::Ok => {
            map.insert("status", "ok");
        }
        SpanStatus::Error { error } => {
            map.insert("status", "error");
            if let Some(error) = error {
                let mut details = JsonMap::new();
                details.insert("name", error.name.as_str());
                details.insert("message", error.message.as_str());
                map.insert("error", Json::object(details));
            }
        }
    }
    Json::object(map).to_string()
}

/// The continuation after a page: `JSON.stringify([started_ms, trace_id])` of its last trace.
pub(crate) fn cursor(started_ms: f64, trace_id: &str) -> String {
    Json::array(vec![Json::from(started_ms), Json::from(trace_id)]).to_string()
}

pub(crate) fn parse_cursor(cursor: &str) -> Result<(f64, String), StoreError> {
    serde_json::from_str(cursor).map_err(|error| StoreError::new(format!("Invalid trace cursor: {error}")))
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::new(format!("Corrupt telemetry row: {error}"))
}

fn parse<T: DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(corrupt)
}

/// Attributes from JSON text. Entries that are not attribute values, such as the `null` upstream writes for a
/// non-finite number, are left out.
fn attributes_from(value: Value) -> Result<SpanAttributes, StoreError> {
    let Value::Object(entries) = value else { return Err(corrupt("attributes are not an object")) };
    let values = entries.into_iter().filter_map(|(key, item)| Some((key, AttributeValue::deserialize(item).ok()?)));
    Ok(values.collect())
}

pub(crate) fn parse_attributes(text: &str) -> Result<SpanAttributes, StoreError> {
    attributes_from(parse(text)?)
}

#[derive(Deserialize)]
struct EventRow {
    name: String,
    time_ms: f64,
    #[serde(default)]
    attributes: Option<Value>,
}

#[derive(Deserialize)]
struct LinkRow {
    trace_id: String,
    span_id: String,
    #[serde(default)]
    attributes: Option<Value>,
}

fn optional_attributes(value: Option<Value>) -> Result<SpanAttributes, StoreError> {
    value.map_or_else(|| Ok(SpanAttributes::new()), attributes_from)
}

pub(crate) fn parse_events(text: &str) -> Result<Vec<StoredEvent>, StoreError> {
    let rows: Vec<EventRow> = parse(text)?;
    let events = rows.into_iter().map(|row| {
        Ok(StoredEvent { name: row.name, time_ms: row.time_ms, attributes: optional_attributes(row.attributes)? })
    });
    events.collect()
}

pub(crate) fn parse_links(text: &str) -> Result<Vec<StoredLink>, StoreError> {
    let rows: Vec<LinkRow> = parse(text)?;
    let links = rows.into_iter().map(|row| {
        let attributes = row.attributes.map(attributes_from).transpose()?;
        Ok(StoredLink { trace_id: row.trace_id, span_id: row.span_id, attributes })
    });
    links.collect()
}

pub(crate) fn parse_status(text: &str) -> Result<SpanStatus, StoreError> {
    parse(text)
}

pub(crate) fn parse_kind(text: &str) -> Result<TelemetryMetricKind, StoreError> {
    serde_json::from_value(Value::String(text.to_owned())).map_err(corrupt)
}
