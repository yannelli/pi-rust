//! Decoding of stored rows, and document materialization from a base and its delta tail.

use pi_chord::delta::{apply, parse_ops};

use super::database::{SqliteExecutor, SqliteValue};
use super::decode;
use super::schema::SqliteTables;
use crate::errors::{DurableError, DurableResult};
use crate::ids::DocumentId;
use crate::storage::scan::MAX_SAFE_INTEGER;
use crate::storage::{is_alive_at, is_current_only};
use crate::types::{DocumentPoint, DocumentRecord, StoredDocument};

pub(crate) async fn records<T: serde::de::DeserializeOwned>(
    executor: &dyn SqliteExecutor,
    sql: &str,
    params: &[SqliteValue],
) -> DurableResult<Vec<T>> {
    let rows = executor.all(sql, params).await?;
    rows.iter().map(|row| decode(row.text("record")?)).collect()
}

pub(crate) async fn record<T: serde::de::DeserializeOwned>(
    executor: &dyn SqliteExecutor,
    sql: &str,
    params: &[SqliteValue],
) -> DurableResult<Option<T>> {
    match executor.get(sql, params).await? {
        Some(row) => Ok(Some(decode(row.text("record")?)?)),
        None => Ok(None),
    }
}

/// Materialize one incarnation from its newest applicable base and the delta tail after it.
pub(crate) async fn materialize(
    executor: &dyn SqliteExecutor,
    t: &SqliteTables,
    id: DocumentId,
    at: DocumentPoint,
) -> DurableResult<Option<StoredDocument>> {
    let id_value = SqliteValue::from(id.0);
    let found: Option<DocumentRecord> =
        record(executor, &format!("SELECT record FROM {} WHERE id = ?", t.documents), std::slice::from_ref(&id_value))
            .await?;
    let Some(stored) = found else { return Ok(None) };
    if at != DocumentPoint::Current && is_current_only(&stored) {
        return Err(DurableError::storage_request(format!("Document {id} does not retain historical content")));
    }
    if !is_alive_at(&stored, at) {
        return Ok(None);
    }
    let upper = SqliteValue::from(match at {
        DocumentPoint::Current => MAX_SAFE_INTEGER,
        DocumentPoint::At(seq) => seq.0,
    });
    let base = executor
        .get(
            &format!(
                "SELECT seq, kind, version, content FROM {}
				WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1",
                t.document_revisions
            ),
            &[id_value.clone(), upper.clone()],
        )
        .await?;
    let Some(base) = base else {
        return Err(DurableError::storage(format!("Document {id} is missing a required base")));
    };
    let version = base.integer("version")?;
    let mut value = decode::<pi_chord::Json>(base.text("content")?)?;
    let tail = executor
        .all(
            &format!(
                "SELECT seq, kind, version, content FROM {}
				WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq",
                t.document_revisions
            ),
            &[id_value, SqliteValue::Integer(base.integer("seq")?), upper],
        )
        .await?;
    for revision in &tail {
        if revision.text("kind")? != "delta" || revision.integer("version")? != version {
            return Err(DurableError::storage(format!(
                "Document {id} crosses a stored version boundary without a base"
            )));
        }
        let ops = parse_ops(&decode(revision.text("content")?)?)?;
        value = apply(Some(value), &ops)?;
    }
    Ok(Some(StoredDocument { record: stored, version: version as u32, value, deltas_since_base: tail.len() as u32 }))
}
