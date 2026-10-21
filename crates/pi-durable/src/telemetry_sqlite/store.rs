//! Store reads over committed telemetry rows (upstream `telemetry-sqlite/store.ts`), with upstream's SQL.

use std::collections::HashMap;
use std::sync::Arc;

use pi_telemetry::{
    AttributeValue, MetricQuery, SpanAttributes, StoreError, StoredMetric, StoredSpan, StoredTrace, TracePage,
    TraceQuery,
};

use super::encode;
use crate::errors::DurableError;
use crate::storage::sqlite::{SqliteDatabase, SqliteExecutor, SqliteRow, SqliteValue, transaction};

const DEFAULT_LIMIT: usize = 20;

fn failed(error: DurableError) -> StoreError {
    StoreError::new(error.to_string())
}

fn column<'a>(row: &'a SqliteRow, name: &str) -> Result<&'a SqliteValue, StoreError> {
    row.get(name).ok_or_else(|| StoreError::new(format!("Telemetry column {name} is missing")))
}

fn text<'a>(row: &'a SqliteRow, name: &str) -> Result<&'a str, StoreError> {
    match column(row, name)? {
        SqliteValue::Text(text) => Ok(text),
        _ => Err(StoreError::new(format!("Telemetry column {name} is not text"))),
    }
}

fn optional_text(row: &SqliteRow, name: &str) -> Result<Option<String>, StoreError> {
    match column(row, name)? {
        SqliteValue::Null => Ok(None),
        _ => text(row, name).map(|text| Some(text.to_owned())),
    }
}

fn number(row: &SqliteRow, name: &str) -> Result<f64, StoreError> {
    match column(row, name)? {
        SqliteValue::Integer(value) => Ok(*value as f64),
        SqliteValue::Real(value) => Ok(*value),
        _ => Err(StoreError::new(format!("Telemetry column {name} is not a number"))),
    }
}

fn stored_span(row: &SqliteRow) -> Result<StoredSpan, StoreError> {
    Ok(StoredSpan {
        id: text(row, "id")?.to_owned(),
        trace_id: text(row, "trace_id")?.to_owned(),
        parent_id: optional_text(row, "parent_id")?,
        name: text(row, "name")?.to_owned(),
        started_ms: number(row, "started_ms")?,
        ended_ms: number(row, "ended_ms")?,
        attributes: encode::parse_attributes(text(row, "attributes")?)?,
        events: encode::parse_events(text(row, "events")?)?,
        links: encode::parse_links(text(row, "links")?)?,
        status: encode::parse_status(text(row, "status")?)?,
    })
}

/// JSON equality must tell boolean `true` from numeric `1`; `json_each` also handles arbitrary attribute keys.
/// Arrays, which upstream queries cannot hold, compare as canonical JSON text.
fn attributes_sql(attributes: &SpanAttributes, column: &str, params: &mut Vec<SqliteValue>) -> String {
    let conditions: Vec<String> = attributes
        .iter()
        .map(|(key, value)| {
            let (kind, bound, compared) = match value {
                AttributeValue::Number(number) => ("entry.type IN ('integer', 'real')", SqliteValue::Real(*number), "?"),
                AttributeValue::Boolean(true) => ("entry.type = 'true'", SqliteValue::Integer(1), "?"),
                AttributeValue::Boolean(false) => ("entry.type = 'false'", SqliteValue::Integer(0), "?"),
                AttributeValue::String(text) => ("entry.type = 'text'", SqliteValue::Text(text.clone()), "?"),
                array => ("entry.type = 'array'", SqliteValue::Text(encode::value_text(array)), "json(?)"),
            };
            params.push(SqliteValue::Text(key.clone()));
            params.push(bound);
            format!(
                "EXISTS (SELECT 1 FROM json_each({column}) entry WHERE entry.key = ? AND {kind} AND entry.value = {compared})"
            )
        })
        .collect();
    if conditions.is_empty() { "1".to_owned() } else { conditions.join(" AND ") }
}

pub(crate) async fn get_trace(db: &dyn SqliteDatabase, trace_id: &str) -> Result<Option<StoredTrace>, StoreError> {
    let sql = "SELECT * FROM telemetry_spans WHERE trace_id = ? ORDER BY started_ms, id";
    let rows = db.all(sql, &[SqliteValue::from(trace_id)]).await.map_err(failed)?;
    if rows.is_empty() {
        return Ok(None);
    }
    let spans = rows.iter().map(stored_span).collect::<Result<_, _>>()?;
    Ok(Some(StoredTrace { trace_id: trace_id.to_owned(), spans }))
}

pub(crate) async fn find_traces(db: &dyn SqliteDatabase, query: TraceQuery) -> Result<TracePage, StoreError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 {
        return Err(StoreError::new("Trace query limit must be positive"));
    }
    let mut conditions: Vec<String> = Vec::new();
    let mut params: Vec<SqliteValue> = Vec::new();
    if let Some(name) = query.name {
        conditions.push(
            "(SELECT name FROM telemetry_spans WHERE trace_id = heads.trace_id ORDER BY started_ms, id LIMIT 1) = ?"
                .into(),
        );
        params.push(SqliteValue::Text(name));
    }
    if let Some(min) = query.start_min {
        conditions.push("started_ms >= ?".into());
        params.push(SqliteValue::Real(min));
    }
    if let Some(max) = query.start_max {
        conditions.push("started_ms <= ?".into());
        params.push(SqliteValue::Real(max));
    }
    if query.errors {
        conditions.push(
            "EXISTS (SELECT 1 FROM telemetry_spans WHERE trace_id = heads.trace_id AND json_extract(status, '$.status') = 'error')".into(),
        );
    }
    if !query.attributes.is_empty() {
        let matched = attributes_sql(&query.attributes, "candidate.attributes", &mut params);
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM telemetry_spans candidate WHERE candidate.trace_id = heads.trace_id AND {matched})"
        ));
    }
    if let Some(cursor) = &query.cursor {
        let (started, trace_id) = encode::parse_cursor(cursor)?;
        conditions.push("(started_ms < ? OR (started_ms = ? AND trace_id > ?))".into());
        params.extend([SqliteValue::Real(started), SqliteValue::Real(started), SqliteValue::Text(trace_id)]);
    }
    let filter = if conditions.is_empty() { "1".to_owned() } else { conditions.join(" AND ") };
    params.push(SqliteValue::Integer(limit as i64 + 1));
    let heads_sql = format!(
        "WITH heads AS (
\t\t\tSELECT trace_id, min(started_ms) started_ms FROM telemetry_spans GROUP BY trace_id
\t\t) SELECT * FROM heads WHERE {filter} ORDER BY started_ms DESC, trace_id LIMIT ?"
    );
    let page = transaction(db, move |tx: Arc<dyn SqliteExecutor>| {
        Box::pin(async move {
            let mut heads = tx.all(&heads_sql, &params).await?;
            let more = heads.len() > limit;
            heads.truncate(limit);
            if heads.is_empty() {
                return Ok((heads, Vec::new(), false));
            }
            let ids: Vec<&str> = heads.iter().filter_map(|head| head.text("trace_id").ok()).collect();
            let ids = serde_json::to_string(&ids).unwrap_or_default();
            let sql = "SELECT * FROM telemetry_spans WHERE trace_id IN (SELECT value FROM json_each(?)) ORDER BY started_ms, id";
            let rows = tx.all(sql, &[SqliteValue::Text(ids)]).await?;
            Ok((heads, rows, more))
        })
    })
    .await
    .map_err(failed)?;
    let (heads, rows, more) = page;
    let mut grouped: HashMap<String, Vec<StoredSpan>> = HashMap::new();
    for row in &rows {
        let span = stored_span(row)?;
        grouped.entry(span.trace_id.clone()).or_default().push(span);
    }
    let mut traces = Vec::with_capacity(heads.len());
    let mut last = None;
    for head in &heads {
        let trace_id = text(head, "trace_id")?.to_owned();
        last = Some((number(head, "started_ms")?, trace_id.clone()));
        let spans = grouped.remove(&trace_id).unwrap_or_default();
        traces.push(StoredTrace { trace_id, spans });
    }
    let cursor = last.filter(|_| more).map(|(started, trace_id)| encode::cursor(started, &trace_id));
    Ok(TracePage { traces, cursor })
}

pub(crate) async fn find_metrics(db: &dyn SqliteDatabase, query: MetricQuery) -> Result<Vec<StoredMetric>, StoreError> {
    let mut params = vec![SqliteValue::Text(query.name)];
    let mut conditions = vec!["name = ?".to_owned()];
    if let Some(min) = query.start_min {
        conditions.push("interval_ms >= ?".into());
        params.push(SqliteValue::Real(min));
    }
    if let Some(max) = query.start_max {
        conditions.push("interval_ms <= ?".into());
        params.push(SqliteValue::Real(max));
    }
    conditions.push(attributes_sql(&query.attributes, "attributes", &mut params));
    let sql =
        format!("SELECT * FROM telemetry_metrics WHERE {} ORDER BY interval_ms, series_key", conditions.join(" AND "));
    let rows = db.all(&sql, &params).await.map_err(failed)?;
    let metrics = rows.iter().map(|row| {
        Ok(StoredMetric {
            interval_ms: number(row, "interval_ms")?,
            name: text(row, "name")?.to_owned(),
            kind: encode::parse_kind(text(row, "kind")?)?,
            unit: optional_text(row, "unit")?,
            attributes: encode::parse_attributes(text(row, "attributes")?)?,
            count: number(row, "count")? as u64,
            sum: number(row, "sum")?,
            min: number(row, "min")?,
            max: number(row, "max")?,
        })
    });
    metrics.collect()
}
