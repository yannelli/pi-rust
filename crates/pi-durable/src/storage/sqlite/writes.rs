//! One `SqliteStorage` commit: validation, table rows, and document revisions in one SQL transaction.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::database::{SqliteExecutor, SqliteValue, transaction};
use super::reads::{materialize, record};
use super::{SqliteStorage, encode, indexed, parse_next_id, scope_columns};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{DocumentId, Seq};
use crate::storage::is_current_only;
use crate::types::{
    DocumentAddress, DocumentContent, DocumentCopySource, DocumentCreate, DocumentRecord, DocumentScope, StorageWrite,
};

#[derive(Clone, Debug, Default)]
struct DocumentAction {
    create: Option<DocumentCreate>,
    copy: Option<DocumentCopySource>,
    content: Option<DocumentContent>,
    retire: bool,
}

fn error(message: String) -> DurableError {
    DurableError::storage(message)
}

fn write_id(write: &StorageWrite) -> Option<u64> {
    match write {
        StorageWrite::Conversation { value } => Some(value.id.0),
        StorageWrite::Entry { value } => Some(value.id.0),
        StorageWrite::Task { value } => Some(value.id.0),
        StorageWrite::Submission { value } => Some(value.id.0),
        StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => Some(record.id.0),
        StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => None,
    }
}

fn address_params(address: &DocumentAddress) -> [SqliteValue; 5] {
    let (scope_kind, owner_id) = scope_columns(&address.scope);
    let key = indexed(address.key.as_deref().unwrap_or(""));
    [indexed(&address.kind), scope_kind, owner_id, address.key.is_some().into(), key]
}

fn prepare_document_actions(writes: &[StorageWrite]) -> DurableResult<Vec<(DocumentId, DocumentAction)>> {
    let mut actions: Vec<(DocumentId, DocumentAction)> = Vec::new();
    let mut positions: HashMap<DocumentId, usize> = HashMap::new();
    for write in writes {
        let id = match write {
            StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => record.id,
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => *id,
            _ => continue,
        };
        let position = *positions.entry(id).or_insert_with(|| {
            actions.push((id, DocumentAction::default()));
            actions.len() - 1
        });
        let action = &mut actions[position].1;
        let more_than_one = || error(format!("Document {id} has more than one content command"));
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one());
                }
                action.create = Some(record.clone());
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentCopy { record, source } => {
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one());
                }
                action.create = Some(record.clone());
                action.copy = Some(*source);
            }
            StorageWrite::DocumentChange { content, .. } => {
                if action.content.is_some() || action.copy.is_some() {
                    return Err(more_than_one());
                }
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentRetire { .. } => {
                if action.retire {
                    return Err(error(format!("Document {id} is retired more than once")));
                }
                action.retire = true;
            }
            _ => unreachable!(),
        }
    }
    Ok(actions)
}

async fn check_global_ids(executor: &dyn SqliteExecutor, writes: &[StorageWrite]) -> DurableResult<()> {
    let mut claimed: HashMap<u64, &'static str> = HashMap::new();
    for write in writes {
        let table = match write {
            StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => continue,
            StorageWrite::DocumentCreate { .. } | StorageWrite::DocumentCopy { .. } => "document",
            StorageWrite::Conversation { .. } => "conversation",
            StorageWrite::Entry { .. } => "entry",
            StorageWrite::Task { .. } => "task",
            StorageWrite::Submission { .. } => "submission",
        };
        let id = write_id(write).expect("a record write has an ID");
        let row = executor.get("SELECT record_type FROM record_ids WHERE id = ?", &[id.into()]).await?;
        let existing = row.map(|row| row.text("record_type").map(str::to_string)).transpose()?;
        let earlier = claimed.get(&id).copied();
        if matches!(table, "conversation" | "entry" | "document") {
            if let Some(existing) = existing {
                return Err(error(format!("ID {id} already belongs to {existing}")));
            }
            if earlier.is_some() {
                return Err(error(format!("ID {id} is written more than once")));
            }
        } else {
            if let Some(existing) = existing.filter(|existing| existing != table) {
                return Err(error(format!("ID {id} already belongs to {existing}")));
            }
            if earlier.is_some_and(|earlier| earlier != table) {
                return Err(error(format!("ID {id} is written as two record types")));
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}

async fn document_record(executor: &dyn SqliteExecutor, id: DocumentId) -> DurableResult<Option<DocumentRecord>> {
    record(executor, "SELECT record FROM documents WHERE id = ?", &[id.0.into()]).await
}

async fn check_document_actions(
    executor: &dyn SqliteExecutor,
    actions: &[(DocumentId, DocumentAction)],
) -> DurableResult<()> {
    let ids: HashSet<DocumentId> = actions.iter().map(|(id, _)| *id).collect();
    let mut live_counts: HashMap<DocumentAddress, i64> = HashMap::new();
    for (id, action) in actions {
        if action.copy.is_some_and(|copy| ids.contains(&copy.id)) {
            return Err(error(format!("Document copy {id} source is changed in the copy batch")));
        }
        let existing = document_record(executor, *id).await?;
        let address = match (&action.create, &existing) {
            (None, None) => return Err(error(format!("Unknown document: {id}"))),
            (Some(_), Some(_)) => return Err(error(format!("Document {id} already exists"))),
            (Some(create), None) => create.address(),
            (None, Some(existing)) => existing.address(),
        };
        if existing.as_ref().is_some_and(|existing| existing.retired_at.is_some()) {
            return Err(error(format!("Document {id} is retired")));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let sql = "SELECT version FROM document_revisions WHERE document_id = ? ORDER BY seq DESC LIMIT 1";
            let Some(last) = executor.get(sql, &[id.0.into()]).await? else {
                return Err(error(format!("Document {id} delta has no base")));
            };
            if last.integer("version")? != i64::from(*version) {
                return Err(error(format!("Document {id} version transition requires a base")));
            }
        }
        let live = match live_counts.get(&address) {
            Some(live) => *live,
            None => {
                let sql = "SELECT id FROM documents
				WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ? AND retired_at IS NULL
				LIMIT 1";
                i64::from(executor.get(sql, &address_params(&address)).await?.is_some())
            }
        };
        let mut live = live;
        if action.retire && existing.is_some() {
            live -= 1;
        }
        if action.create.is_some() && !action.retire {
            live += 1;
        }
        live_counts.insert(address, live);
    }
    if live_counts.values().any(|live| *live > 1) {
        return Err(error("Document address already has a current incarnation".into()));
    }
    Ok(())
}

async fn claim_id(executor: &dyn SqliteExecutor, id: u64, table: &str) -> DurableResult<()> {
    executor.run("INSERT OR IGNORE INTO record_ids (id, record_type) VALUES (?, ?)", &[id.into(), table.into()]).await
}

async fn apply_table_write(executor: &dyn SqliteExecutor, write: &StorageWrite, seq: Seq) -> DurableResult<()> {
    match write {
        StorageWrite::Conversation { value } => {
            claim_id(executor, value.id.0, "conversation").await?;
            let sql =
                "INSERT INTO conversations (id, owner_conversation_id, owner_task_id, record) VALUES (?, ?, ?, ?)";
            let owner = value.owner;
            let params = [
                value.id.0.into(),
                owner.map(|owner| owner.conversation_id.0).into(),
                owner.map(|owner| owner.task_id.0).into(),
                encode(value)?.into(),
            ];
            executor.run(sql, &params).await
        }
        StorageWrite::Entry { value } => {
            claim_id(executor, value.id.0, "entry").await?;
            let sql = "INSERT INTO entries (id, conversation_id, head, commit_seq, record) VALUES (?, ?, ?, ?, ?)";
            let params = [
                value.id.0.into(),
                value.conversation_id.0.into(),
                value.head.map(|head| head.0).into(),
                seq.0.into(),
                encode(value)?.into(),
            ];
            executor.run(sql, &params).await
        }
        StorageWrite::Task { value } => {
            claim_id(executor, value.id.0, "task").await?;
            let sql = "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record)
						VALUES (?, ?, ?, ?, ?, ?, ?)
						ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, kind = excluded.kind,
						status = excluded.status, abort_requested = excluded.abort_requested,
						background = excluded.background, record = excluded.record";
            let params = [
                value.id.0.into(),
                value.conversation_id.0.into(),
                indexed(&value.kind),
                value.status().as_str().into(),
                value.abort_requested.into(),
                value.background.into(),
                encode(value)?.into(),
            ];
            executor.run(sql, &params).await
        }
        StorageWrite::Submission { value } => {
            claim_id(executor, value.id.0, "submission").await?;
            let sql = "INSERT INTO submissions (id, conversation_id, request_id, status, record) VALUES (?, ?, ?, ?, ?)
						ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id,
						request_id = excluded.request_id, status = excluded.status, record = excluded.record";
            let request_id = value.request_id.as_deref().map_or(SqliteValue::Null, indexed);
            let params = [
                value.id.0.into(),
                value.conversation_id.0.into(),
                request_id,
                value.status.as_str().into(),
                encode(value)?.into(),
            ];
            executor.run(sql, &params).await
        }
        _ => Ok(()),
    }
}

async fn copy_content(
    executor: &dyn SqliteExecutor,
    create: &DocumentCreate,
    source: DocumentCopySource,
) -> DurableResult<DocumentContent> {
    let Some(stored) = materialize(executor, source.id, source.at).await? else {
        return Err(error(format!("Fork source document {} cannot be read", source.id)));
    };
    let matches = matches!(stored.record.scope, DocumentScope::Conversation { .. })
        && matches!(create.scope, DocumentScope::Conversation { .. })
        && stored.record.kind == create.kind
        && stored.record.key == create.key
        && stored.record.history == create.history
        && stored.record.fork == create.fork;
    if !matches {
        return Err(error(format!("Fork source document {} does not match the copied record", source.id)));
    }
    Ok(DocumentContent::Base { version: stored.version, value: stored.value })
}

async fn apply_document_action(
    executor: &dyn SqliteExecutor,
    id: DocumentId,
    action: &DocumentAction,
    seq: Seq,
) -> DurableResult<()> {
    let mut content = action.content.clone();
    if let (Some(source), Some(create)) = (action.copy, &action.create) {
        let copied = copy_content(executor, create, source).await;
        content = Some(copied.map_err(|cause| error(format!("Document copy {id} was rejected: {cause}")))?);
    }
    let mut stored = match &action.create {
        Some(create) => {
            let mut stored = create.clone().into_record(seq);
            if action.retire {
                stored.retired_at = Some(seq);
            }
            claim_id(executor, id.0, "document").await?;
            let [kind, scope_kind, owner_id, family, key_value] = address_params(&stored.address());
            let sql = "INSERT INTO documents
						(id, kind, family, key_value, scope_kind, owner_id, created_at, retired_at, record)
						VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)";
            let retired = action.retire.then_some(seq.0);
            let params = [
                id.0.into(),
                kind,
                family,
                key_value,
                scope_kind,
                owner_id,
                seq.0.into(),
                retired.into(),
                encode(&stored)?.into(),
            ];
            executor.run(sql, &params).await?;
            stored
        }
        None => document_record(executor, id).await?.ok_or_else(|| error(format!("Unknown document: {id}")))?,
    };
    if let Some(content) = &content {
        if matches!(content, DocumentContent::Base { .. }) && is_current_only(&stored) {
            executor.run("DELETE FROM document_revisions WHERE document_id = ?", &[id.0.into()]).await?;
        }
        let (kind, encoded) = match content {
            DocumentContent::Base { value, .. } => ("base", encode(value)?),
            DocumentContent::Delta { ops, .. } => ("delta", encode(ops)?),
        };
        let sql = "INSERT INTO document_revisions (document_id, seq, kind, version, content) VALUES (?, ?, ?, ?, ?)";
        let params = [id.0.into(), seq.0.into(), kind.into(), content.version().into(), encoded.into()];
        executor.run(sql, &params).await?;
    }
    if action.retire {
        if action.create.is_none() {
            stored.retired_at = Some(seq);
            let sql = "UPDATE documents SET retired_at = ?, record = ? WHERE id = ?";
            executor.run(sql, &[seq.0.into(), encode(&stored)?.into(), id.0.into()]).await?;
        }
        if is_current_only(&stored) {
            executor.run("DELETE FROM document_revisions WHERE document_id = ?", &[id.0.into()]).await?;
        }
    }
    Ok(())
}

impl SqliteStorage {
    pub(crate) async fn commit_writes(&self, writes: &[StorageWrite]) -> DurableResult<Seq> {
        self.assert_open()?;
        let actions = Arc::new(prepare_document_actions(writes)?);
        let candidate = writes.iter().filter_map(write_id).fold(self.next_id(), |next, id| next.max(id + 1));
        let seq = transaction(&*self.db, move |executor| {
            Box::pin(async move {
                let executor = &*executor;
                let sql = "SELECT next_id, next_seq FROM durable_metadata WHERE singleton = 1";
                let metadata = executor.get(sql, &[]).await?;
                let metadata = metadata.ok_or_else(|| DurableError::storage("Durable SQLite metadata is missing"))?;
                let stored_next_id = parse_next_id(metadata.text("next_id")?)?;
                let seq = Seq(metadata.integer("next_seq")? as u64);
                check_global_ids(executor, writes).await?;
                check_document_actions(executor, &actions).await?;
                for write in writes {
                    apply_table_write(executor, write, seq).await?;
                }
                for (id, action) in actions.iter() {
                    apply_document_action(executor, *id, action, seq).await?;
                }
                let next_id = stored_next_id.max(candidate).to_string();
                let sql = "UPDATE durable_metadata SET next_id = ?, next_seq = ? WHERE singleton = 1";
                executor.run(sql, &[next_id.into(), (seq.0 + 1).into()]).await?;
                Ok(seq)
            })
        })
        .await?;
        let mut next_id = self.next_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        *next_id = (*next_id).max(candidate);
        Ok(seq)
    }
}
