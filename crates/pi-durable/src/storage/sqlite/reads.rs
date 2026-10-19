//! The `Storage` implementation of `SqliteStorage`: indexed reads and scans.

use std::sync::atomic::Ordering;

use async_trait::async_trait;
use pi_chord::Context;

use super::database::{SqliteValue, transaction};
use super::rows::{materialize, record, records};
use super::{SqliteStorage, decode, indexed, scope_columns};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::storage::memory::page;
use crate::storage::scan::{MAX_SAFE_INTEGER, ScanStart, cursor_id, scan_start};
use crate::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint, DocumentQuery, DocumentRecord,
    EntryQuery, EntryRecord, Page, ScanOrder, Storage, StorageWrite, StoredDocument, StoredEntry, SubmissionQuery,
    SubmissionRecord, TaskQuery, TaskRecord,
};

/// The ID condition, its parameter, and the `ORDER BY` direction of a table scan.
fn scan_sql(start: ScanStart) -> (&'static str, SqliteValue, &'static str) {
    match start.order {
        ScanOrder::Ascending => ("id > ?", SqliteValue::Integer(start.after.unwrap_or(-1)), "ASC"),
        ScanOrder::Descending => {
            ("id < ?", SqliteValue::Integer(start.after.unwrap_or(MAX_SAFE_INTEGER as i64)), "DESC")
        }
    }
}

fn unknown_conversation(id: ConversationId) -> DurableError {
    DurableError::storage_request(format!("Unknown conversation: {id}"))
}

impl SqliteStorage {
    async fn read_conversation(&self, id: ConversationId) -> DurableResult<Option<ConversationRecord>> {
        record(
            &*self.db,
            &format!("SELECT record FROM {} WHERE id = ?", self.schema.tables.conversations),
            &[id.0.into()],
        )
        .await
    }

    async fn ancestor(&self, id: ConversationId) -> DurableResult<ConversationRecord> {
        self.read_conversation(id).await?.ok_or_else(|| DurableError::storage(format!("Missing conversation {id}")))
    }

    async fn entries_descending(
        &self,
        query: &EntryQuery,
        limit: usize,
        after: Option<i64>,
    ) -> DurableResult<Vec<EntryRecord>> {
        let mut conversation = self
            .read_conversation(query.conversation_id)
            .await?
            .ok_or_else(|| unknown_conversation(query.conversation_id))?;
        let mut upper = query.max_entry_id.map(|id| id.0 as i64);
        if let Some(after) = after {
            upper = Some(upper.unwrap_or(MAX_SAFE_INTEGER as i64).min(after - 1));
        }
        let mut values: Vec<EntryRecord> = Vec::new();
        loop {
            let mut clauses = vec!["conversation_id = ?"];
            let mut params = vec![SqliteValue::from(conversation.id.0)];
            if let Some(min) = query.min_entry_id {
                clauses.push("id >= ?");
                params.push(min.0.into());
            }
            if let Some(upper) = upper {
                clauses.push("id <= ?");
                params.push(upper.into());
            }
            params.push(((limit + 1 - values.len()) as i64).into());
            let sql = format!(
                "SELECT record FROM {} WHERE {} ORDER BY id DESC LIMIT ?",
                self.schema.tables.entries,
                clauses.join(" AND ")
            );
            values.extend(records::<EntryRecord>(&*self.db, &sql, &params).await?);
            let Some(parent) = conversation.parent else { break };
            if values.len() > limit {
                break;
            }
            let capped = upper.map_or(parent.at.0 as i64, |upper| upper.min(parent.at.0 as i64));
            upper = Some(capped);
            if query.min_entry_id.is_some_and(|min| capped < min.0 as i64) {
                break;
            }
            conversation = self.ancestor(parent.conversation_id).await?;
        }
        Ok(values)
    }

    /// Oldest first: the fork chain's segments from the root conversation forward, each up to its fork point.
    async fn entries_ascending(
        &self,
        query: &EntryQuery,
        limit: usize,
        after: Option<i64>,
    ) -> DurableResult<Vec<EntryRecord>> {
        let mut conversation = self
            .read_conversation(query.conversation_id)
            .await?
            .ok_or_else(|| unknown_conversation(query.conversation_id))?;
        let mut upper = query.max_entry_id.map(|id| id.0 as i64);
        let mut segments = Vec::new();
        loop {
            segments.push((conversation.id, upper));
            let Some(parent) = conversation.parent else { break };
            let capped = upper.map_or(parent.at.0 as i64, |upper| upper.min(parent.at.0 as i64));
            upper = Some(capped);
            if query.min_entry_id.is_some_and(|min| capped < min.0 as i64) {
                break;
            }
            conversation = self.ancestor(parent.conversation_id).await?;
        }
        let mut lower = query.min_entry_id.map(|id| id.0 as i64);
        if let Some(after) = after {
            lower = Some(lower.unwrap_or(after + 1).max(after + 1));
        }
        let mut values: Vec<EntryRecord> = Vec::new();
        for (conversation_id, upper) in segments.into_iter().rev() {
            let mut clauses = vec!["conversation_id = ?"];
            let mut params = vec![SqliteValue::from(conversation_id.0)];
            if let Some(lower) = lower {
                clauses.push("id >= ?");
                params.push(lower.into());
            }
            if let Some(upper) = upper {
                clauses.push("id <= ?");
                params.push(upper.into());
            }
            params.push(((limit + 1 - values.len()) as i64).into());
            let sql = format!(
                "SELECT record FROM {} WHERE {} ORDER BY id ASC LIMIT ?",
                self.schema.tables.entries,
                clauses.join(" AND ")
            );
            values.extend(records::<EntryRecord>(&*self.db, &sql, &params).await?);
            if values.len() > limit {
                break;
            }
        }
        Ok(values)
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    async fn commit(&self, writes: &[StorageWrite], _context: &Context) -> DurableResult<Seq> {
        self.commit_writes(writes).await
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.assert_open()?;
        let mut next_id = self.next_id.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if *next_id > MAX_SAFE_INTEGER {
            return Err(DurableError::storage("ID space is exhausted"));
        }
        *next_id += 1;
        Ok(*next_id - 1)
    }

    async fn conversation(&self, id: ConversationId, _context: &Context) -> DurableResult<Option<ConversationRecord>> {
        let _read = self.admit_read()?;
        self.read_conversation(id).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        let _read = self.admit_read()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause];
        let mut params = vec![param];
        if let Some(owner) = query.owner_conversation_id {
            clauses.push("owner_conversation_id = ?");
            params.push(owner.0.into());
        }
        if let Some(owner) = query.owner_task_id {
            clauses.push("owner_task_id = ?");
            params.push(owner.0.into());
        }
        params.push((limit as i64 + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {direction} LIMIT ?",
            self.schema.tables.conversations,
            clauses.join(" AND ")
        );
        let values = records(&*self.db, &sql, &params).await?;
        Ok(page(values, limit, start.order, |record: &ConversationRecord| record.id.0))
    }

    async fn entry(&self, id: EntryId, _context: &Context) -> DurableResult<Option<StoredEntry>> {
        let _read = self.admit_read()?;
        let row = self
            .db
            .get(&format!("SELECT record, commit_seq FROM {} WHERE id = ?", self.schema.tables.entries), &[id.0.into()])
            .await?;
        let Some(row) = row else { return Ok(None) };
        Ok(Some(StoredEntry {
            entry: decode(row.text("record")?)?,
            commit_seq: Seq(row.integer("commit_seq")? as u64),
        }))
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        let _read = self.admit_read()?;
        let mut conversation =
            self.read_conversation(conversation_id).await?.ok_or_else(|| unknown_conversation(conversation_id))?;
        let Some(stored) = self.entry(id, context).await? else { return Ok(None) };
        let mut upper = u64::MAX;
        while conversation.id != stored.entry.conversation_id {
            let Some(parent) = conversation.parent else { return Ok(None) };
            upper = upper.min(parent.at.0);
            conversation = self.ancestor(parent.conversation_id).await?;
        }
        Ok((stored.entry.id.0 <= upper).then_some(stored))
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        _context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        let _read = self.admit_read()?;
        let mut conversation =
            self.read_conversation(conversation_id).await?.ok_or_else(|| unknown_conversation(conversation_id))?;
        let mut upper = at_or_before.map(|id| id.0);
        loop {
            let found: Option<EntryRecord> = match upper {
                None => {
                    let sql = &format!(
                        "SELECT record FROM {} WHERE conversation_id = ? AND head IS NOT NULL ORDER BY id DESC LIMIT 1",
                        self.schema.tables.entries
                    );
                    record(&*self.db, sql, &[conversation.id.0.into()]).await?
                }
                Some(upper) => {
                    let sql = &format!(
                        "SELECT record FROM {} WHERE conversation_id = ? AND head IS NOT NULL AND id <= ? ORDER BY id DESC LIMIT 1",
                        self.schema.tables.entries
                    );
                    record(&*self.db, sql, &[conversation.id.0.into(), upper.into()]).await?
                }
            };
            if found.is_some() {
                return Ok(found);
            }
            let Some(parent) = conversation.parent else { return Ok(None) };
            upper = Some(upper.map_or(parent.at.0, |upper| upper.min(parent.at.0)));
            conversation = self.ancestor(parent.conversation_id).await?;
        }
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        let _read = self.admit_read()?;
        let start = scan_start(query.order, cursor, ScanOrder::Descending)?;
        let values = match start.order {
            ScanOrder::Ascending => self.entries_ascending(query, limit, start.after).await?,
            ScanOrder::Descending => self.entries_descending(query, limit, start.after).await?,
        };
        Ok(page(values, limit, start.order, |entry: &EntryRecord| entry.id.0))
    }

    async fn task(&self, id: TaskId, _context: &Context) -> DurableResult<Option<TaskRecord>> {
        let _read = self.admit_read()?;
        record(&*self.db, &format!("SELECT record FROM {} WHERE id = ?", self.schema.tables.tasks), &[id.0.into()])
            .await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        let _read = self.admit_read()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause];
        let mut params = vec![param];
        if let Some(conversation_id) = query.conversation_id {
            clauses.push("conversation_id = ?");
            params.push(conversation_id.0.into());
        }
        if let Some(kind) = &query.kind {
            clauses.push("kind = ?");
            params.push(indexed(kind));
        }
        if let Some(status) = query.status {
            clauses.push("status = ?");
            params.push(status.as_str().into());
        }
        if let Some(abort_requested) = query.abort_requested {
            clauses.push("abort_requested = ?");
            params.push(abort_requested.into());
        }
        if let Some(background) = query.background {
            clauses.push("background = ?");
            params.push(background.into());
        }
        params.push((limit as i64 + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {direction} LIMIT ?",
            self.schema.tables.tasks,
            clauses.join(" AND ")
        );
        let values = records(&*self.db, &sql, &params).await?;
        Ok(page(values, limit, start.order, |record: &TaskRecord| record.id.0))
    }

    async fn submission(&self, id: SubmissionId, _context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        let _read = self.admit_read()?;
        record(
            &*self.db,
            &format!("SELECT record FROM {} WHERE id = ?", self.schema.tables.submissions),
            &[id.0.into()],
        )
        .await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        let _read = self.admit_read()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (clause, param, direction) = scan_sql(start);
        let mut clauses = vec![clause];
        let mut params = vec![param];
        if let Some(conversation_id) = query.conversation_id {
            clauses.push("conversation_id = ?");
            params.push(conversation_id.0.into());
        }
        if let Some(status) = query.status {
            clauses.push("status = ?");
            params.push(status.as_str().into());
        }
        params.push((limit as i64 + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id {direction} LIMIT ?",
            self.schema.tables.submissions,
            clauses.join(" AND ")
        );
        let values = records(&*self.db, &sql, &params).await?;
        Ok(page(values, limit, start.order, |record: &SubmissionRecord| record.id.0))
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        _context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        let _read = self.admit_read()?;
        let sql = &format!(
            "SELECT record FROM {} WHERE conversation_id = ? AND request_id = ?",
            self.schema.tables.submissions
        );
        record(&*self.db, sql, &[conversation_id.0.into(), indexed(request_id)]).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        let _read = self.admit_read()?;
        let (scope_kind, owner_id) = scope_columns(&address.scope);
        let mut params = vec![
            indexed(&address.kind),
            scope_kind,
            owner_id,
            address.key.is_some().into(),
            indexed(address.key.as_deref().unwrap_or("")),
        ];
        let sql = match at {
            DocumentPoint::Current => {
                format!(
                    "SELECT record FROM {}
					WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
					AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
                    self.schema.tables.documents
                )
            }
            DocumentPoint::At(seq) => {
                params.extend([seq.0.into(), seq.0.into()]);
                format!(
                    "SELECT record FROM {}
					WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
					AND created_at <= ? AND (retired_at IS NULL OR retired_at > ?)
					ORDER BY created_at DESC LIMIT 1",
                    self.schema.tables.documents
                )
            }
        };
        record(&*self.db, &sql, &params).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        let _read = self.admit_read()?;
        // The record and revision queries must see one committed state; a commit between them can replace the base.
        let tables = &self.schema.tables;
        transaction(&*self.db, move |executor| Box::pin(async move { materialize(&*executor, tables, id, at).await }))
            .await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        let _read = self.admit_read()?;
        let (scope_kind, owner_id) = scope_columns(&query.scope);
        let mut clauses = vec!["scope_kind = ?", "owner_id = ?", "id > ?"];
        let mut params = vec![scope_kind, owner_id, cursor_id(cursor)?.unwrap_or(-1).into()];
        if let Some(kind) = &query.kind {
            clauses.push("kind = ?");
            params.push(indexed(kind));
        }
        match query.at {
            DocumentPoint::Current => clauses.push("retired_at IS NULL"),
            DocumentPoint::At(seq) => {
                clauses.extend(["created_at <= ?", "(retired_at IS NULL OR retired_at > ?)"]);
                params.extend([seq.0.into(), seq.0.into()]);
            }
        }
        params.push((limit as i64 + 1).into());
        let sql = format!(
            "SELECT record FROM {} WHERE {} ORDER BY id LIMIT ?",
            self.schema.tables.documents,
            clauses.join(" AND ")
        );
        let values = records(&*self.db, &sql, &params).await?;
        Ok(page(values, limit, ScanOrder::Ascending, |record: &DocumentRecord| record.id.0))
    }

    async fn close(&self, _context: &Context) -> DurableResult<()> {
        self.closed.store(true, Ordering::SeqCst);
        self.closing.get_or_init(|| self.close_database()).await.clone()
    }
}
