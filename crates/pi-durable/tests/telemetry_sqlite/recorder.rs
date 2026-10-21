//! Upstream `describe("SQLite telemetry span and propagation conformance")`: the pi-telemetry adapter cases, read
//! back from the committed rows as upstream's fixture does.

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_telemetry::testing::{TelemetryAdapterFixture, create_telemetry_adapter_conformance};
use pi_telemetry::{
    RecordedTelemetryEvent, RecordedTelemetryLink, RecordedTelemetrySpan, SpanAttributes, TelemetryCarrier,
    TelemetryContext,
};
use serde_json::Value;

use crate::support::Fixture;

struct SqliteFixture(Fixture);

fn carrier(span: &str, trace: &str) -> TelemetryCarrier {
    TelemetryCarrier::from([("pi-sqlite-span".into(), span.into()), ("pi-sqlite-trace".into(), trace.into())])
}

fn json(row: &Value, column: &str) -> Value {
    serde_json::from_str(row[column].as_str().unwrap()).unwrap()
}

fn attributes(value: &Value) -> SpanAttributes {
    serde_json::from_value(value.clone()).unwrap()
}

/// A stored row as the in-memory recorder's snapshot: local counters as IDs and the row ID as end sequence.
fn recorded(row: &Value) -> RecordedTelemetrySpan {
    let (id, trace) = (row["id"].as_str().unwrap(), row["trace_id"].as_str().unwrap());
    let (scope, counter) = id.split_once(':').unwrap();
    let parent = row["parent_id"].as_str();
    let local = parent.and_then(|parent| parent.split_once(':')).filter(|(parent_scope, _)| *parent_scope == scope);
    let (events, links) = (json(row, "events"), json(row, "links"));
    let events = events.as_array().unwrap().iter().map(|event| RecordedTelemetryEvent {
        name: event["name"].as_str().unwrap().into(),
        attributes: attributes(&event["attributes"]),
    });
    let links = links.as_array().unwrap().iter().map(|link| RecordedTelemetryLink {
        context: carrier(link["span_id"].as_str().unwrap(), link["trace_id"].as_str().unwrap()),
        attributes: attributes(&link["attributes"]),
    });
    RecordedTelemetrySpan {
        id: counter.parse().unwrap(),
        parent_id: local.and_then(|(_, counter)| counter.parse().ok()),
        context: carrier(id, trace),
        parent_context: parent.map(|parent| carrier(parent, trace)),
        links: links.collect(),
        name: row["name"].as_str().unwrap().into(),
        attributes: attributes(&json(row, "attributes")),
        events: events.collect(),
        status: serde_json::from_value(json(row, "status")).unwrap(),
        settled: true,
        end_sequence: row["rowid"].as_u64(),
    }
}

impl TelemetryAdapterFixture for SqliteFixture {
    fn context(&self) -> Arc<dyn TelemetryContext> {
        Arc::new(self.0.telemetry.clone())
    }

    fn get_spans(&self) -> BoxFuture<'_, Vec<RecordedTelemetrySpan>> {
        Box::pin(async move {
            self.0.flush().await;
            let rows = self.0.db.rows("SELECT rowid, * FROM telemetry_spans").await;
            let mut spans: Vec<RecordedTelemetrySpan> = rows.as_array().unwrap().iter().map(recorded).collect();
            spans.sort_by_key(|span| span.id);
            spans
        })
    }
}

#[tokio::test(start_paused = true)]
async fn sqlite_telemetry_passes_span_and_propagation_conformance() {
    let cases = create_telemetry_adapter_conformance(Arc::new(|| {
        Box::pin(async { Box::new(SqliteFixture(Fixture::new(Default::default()).await)) as Box<_> })
    }));
    assert!(cases.len() >= 7);
    for case in cases.iter().filter(|case| case.group != "metrics") {
        if let Err(error) = case.run().await {
            panic!("{} / {}: {error}", case.group, case.name);
        }
    }
}
