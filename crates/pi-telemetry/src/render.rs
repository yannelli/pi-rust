//! Plain-text trace rendering over stored spans (upstream `render.ts`, the `/render` subpath).

use std::collections::HashMap;
use std::fmt::Write;

use crate::attributes::{AttributeValue, SpanAttributes};
use crate::span::SpanStatus;
use crate::store::StoredSpan;

/// A number as JavaScript prints it: integral values without a fraction.
fn js_number(value: f64) -> String {
    if value == 0.0 {
        "0".into()
    } else if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""))
}

fn json_number(value: f64) -> String {
    if value.is_finite() { js_number(value) } else { "null".into() }
}

fn json_list<T>(items: &[T], encode: impl Fn(&T) -> String) -> String {
    format!("[{}]", items.iter().map(encode).collect::<Vec<_>>().join(","))
}

fn json_value(value: &AttributeValue) -> String {
    match value {
        AttributeValue::String(text) => json_string(text),
        AttributeValue::Number(number) => json_number(*number),
        AttributeValue::Boolean(flag) => flag.to_string(),
        AttributeValue::StringArray(items) => json_list(items, |item| json_string(item)),
        AttributeValue::NumberArray(items) => json_list(items, |item| json_number(*item)),
        AttributeValue::BooleanArray(items) => json_list(items, bool::to_string),
    }
}

fn format_attributes(attributes: &SpanAttributes) -> String {
    attributes.iter().fold(String::new(), |mut text, (key, value)| {
        let _ = write!(text, " {key}={}", json_value(value));
        text
    })
}

/// Render stored spans as a chronological forest, treating absent parents as roots. Spans are deduplicated by
/// ID, attributes print in key order, and links are printed but never followed.
pub fn render_trace(spans: &[StoredSpan]) -> String {
    let mut positions: HashMap<&str, usize> = HashMap::new();
    let mut unique: Vec<&StoredSpan> = Vec::new();
    for span in spans {
        match positions.get(span.id.as_str()) {
            Some(&index) => unique[index] = span,
            None => {
                positions.insert(&span.id, unique.len());
                unique.push(span);
            }
        }
    }
    unique.sort_by(|left, right| left.started_ms.total_cmp(&right.started_ms));
    let mut roots: Vec<&StoredSpan> = Vec::new();
    let mut children: HashMap<&str, Vec<&StoredSpan>> = HashMap::new();
    for span in unique {
        match span.parent_id.as_deref() {
            Some(parent) if positions.contains_key(parent) => children.entry(parent).or_default().push(span),
            _ => roots.push(span),
        }
    }

    let mut lines: Vec<String> = Vec::new();
    let mut pending: Vec<(&StoredSpan, usize)> = roots.into_iter().rev().map(|span| (span, 0)).collect();
    while let Some((span, depth)) = pending.pop() {
        let indent = "  ".repeat(depth);
        let mut error = String::new();
        if let SpanStatus::Error { error: details } = &span.status {
            error.push_str(" error");
            if let Some(details) = details {
                let _ = write!(error, " {}: {}", details.name, details.message);
            }
        }
        let duration = js_number(span.ended_ms - span.started_ms);
        lines.push(format!("{indent}{}{} ({duration}ms){error}", span.name, format_attributes(&span.attributes)));
        for event in &span.events {
            let attributes = format_attributes(&event.attributes);
            lines.push(format!("{indent}  · {}{attributes} (+{}ms)", event.name, js_number(event.time_ms)));
        }
        for link in &span.links {
            let attributes = link.attributes.as_ref().map(format_attributes).unwrap_or_default();
            lines.push(format!("{indent}  → link trace {} span {}{attributes}", link.trace_id, link.span_id));
        }
        for child in children.get(span.id.as_str()).into_iter().flatten().rev() {
            pending.push((child, depth + 1));
        }
    }
    lines.join("\n")
}
