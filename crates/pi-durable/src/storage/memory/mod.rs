//! Detached in-memory reference implementation of `Storage` (spec section 11.1).
//!
//! Stored writes and returned reads are owned copies, like the ownership boundary of serialization-backed stores.

mod commit;
mod state;

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use pi_chord::Context;

pub use commit::PreparedMemoryCommit;
use state::is_alive_at;

use super::scan::{MAX_SAFE_INTEGER, cursor_id, next_cursor, scan_start};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::types::{
    ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentPoint, DocumentQuery, DocumentRecord,
    EntryQuery, EntryRecord, Page, ScanOrder, Storage, StorageWrite, StoredDocument, StoredEntry, SubmissionQuery,
    SubmissionRecord, TaskQuery, TaskRecord,
};
use state::{State, scan_indexes, upper_bound};

struct Inner {
    state: State,
    next_id: u64,
    next_seq: u64,
    closed: bool,
}

/// In-memory `Storage`: the reference semantics every backend matches.
pub struct MemoryStorage {
    inner: Mutex<Inner>,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

/// The first `limit` values, and a cursor after the last one when more follow.
pub(crate) fn page<T>(mut values: Vec<T>, limit: usize, order: ScanOrder, id: impl Fn(&T) -> u64) -> Page<T> {
    if values.len() <= limit {
        return Page::new(values, None);
    }
    values.truncate(limit);
    let next = values.last().map(|last| next_cursor(id(last), order));
    Page::new(values, next)
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { state: State::default(), next_id: 2, next_seq: 1, closed: false }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn open(&self) -> DurableResult<std::sync::MutexGuard<'_, Inner>> {
        let inner = self.lock();
        if inner.closed {
            return Err(DurableError::storage("MemoryStorage is closed"));
        }
        Ok(inner)
    }

    /// Validate and detach one commit without changing observable state. `seq` defaults to the next sequence.
    pub fn prepare_commit(&self, writes: &[StorageWrite], seq: Option<Seq>) -> DurableResult<PreparedMemoryCommit> {
        let inner = self.open()?;
        let seq = seq.unwrap_or(Seq(inner.next_seq));
        if seq.0 < inner.next_seq || seq.0 > MAX_SAFE_INTEGER {
            return Err(DurableError::storage(format!("Commit sequence {seq} does not strictly increase")));
        }
        let writes = inner.state.resolve_document_copies(writes.to_vec())?;
        inner.state.check_global_ids(&writes)?;
        let actions = State::prepare_document_actions(&writes)?;
        inner.state.check_document_actions(&actions)?;
        Ok(PreparedMemoryCommit { seq, writes, actions, applied: AtomicBool::new(false) })
    }

    /// Apply a prepared commit once; applying it again only returns its sequence.
    pub fn apply(&self, prepared: &PreparedMemoryCommit) -> Seq {
        if prepared.take() {
            let mut inner = self.lock();
            let seq = prepared.seq;
            let written = inner.state.apply_writes(&prepared.writes, seq);
            let created = inner.state.apply_document_actions(&prepared.actions, seq);
            inner.next_id = inner.next_id.max(written.max(created) + 1);
            inner.next_seq = seq.0 + 1;
        }
        prepared.seq
    }

    pub(crate) fn mint_id_sync(&self) -> DurableResult<u64> {
        let mut inner = self.open()?;
        if inner.next_id > MAX_SAFE_INTEGER {
            return Err(DurableError::storage("ID space is exhausted"));
        }
        inner.next_id += 1;
        Ok(inner.next_id - 1)
    }

    pub(crate) fn document_sync(&self, id: DocumentId, at: DocumentPoint) -> DurableResult<Option<StoredDocument>> {
        self.open()?.state.materialize(id, at)
    }

    pub(crate) fn close_sync(&self) {
        self.lock().closed = true;
    }
}

#[async_trait]
impl Storage for MemoryStorage {
    async fn commit(&self, writes: &[StorageWrite], _context: &Context) -> DurableResult<Seq> {
        let prepared = self.prepare_commit(writes, None)?;
        Ok(self.apply(&prepared))
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.mint_id_sync()
    }

    async fn conversation(&self, id: ConversationId, _context: &Context) -> DurableResult<Option<ConversationRecord>> {
        Ok(self.open()?.state.conversations.get(&id).copied())
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        let inner = self.open()?;
        let state = &inner.state;
        let ids = if let Some(task_id) = query.owner_task_id {
            state.conversation_ids_by_owner_task.get(&task_id)
        } else if let Some(conversation_id) = query.owner_conversation_id {
            state.conversation_ids_by_owner_conversation.get(&conversation_id)
        } else {
            Some(&state.conversation_ids)
        };
        let ids = ids.map(Vec::as_slice).unwrap_or_default();
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let mut values = Vec::new();
        for index in scan_indexes(ids, start.order == ScanOrder::Ascending, start.after) {
            if values.len() > limit {
                break;
            }
            let value = state.conversations[&ConversationId(ids[index])];
            if let Some(owner) = query.owner_conversation_id
                && value.owner.map(|owner| owner.conversation_id) != Some(owner)
            {
                continue;
            }
            values.push(value);
        }
        Ok(page(values, limit, start.order, |value| value.id.0))
    }

    async fn entry(&self, id: EntryId, _context: &Context) -> DurableResult<Option<StoredEntry>> {
        let inner = self.open()?;
        Ok(inner
            .state
            .entries
            .get(&id)
            .map(|entry| StoredEntry { entry: entry.clone(), commit_seq: inner.state.entry_commit_seqs[&id] }))
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        _context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        let inner = self.open()?;
        let found = inner.state.visible_entries(conversation_id, id.0 as i64, id.0 as i64, 1)?;
        Ok(found
            .first()
            .map(|entry| StoredEntry { entry: (*entry).clone(), commit_seq: inner.state.entry_commit_seqs[&id] }))
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        _context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        let inner = self.open()?;
        Ok(inner.state.latest_head_marker(conversation_id, at_or_before)?.cloned())
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        let inner = self.open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Descending)?;
        let mut min = query.min_entry_id.map_or(i64::MIN, |id| id.0 as i64);
        let mut max = query.max_entry_id.map_or(i64::MAX, |id| id.0 as i64);
        match (start.after, start.order) {
            (Some(after), ScanOrder::Descending) => max = max.min(after - 1),
            (Some(after), ScanOrder::Ascending) => min = min.max(after + 1),
            (None, _) => {}
        }
        let stop_after = limit.saturating_add(1);
        let found = match start.order {
            ScanOrder::Descending => inner.state.visible_entries(query.conversation_id, min, max, stop_after)?,
            ScanOrder::Ascending => {
                inner.state.visible_entries_ascending(query.conversation_id, min, max, stop_after)?
            }
        };
        let values = found.into_iter().cloned().collect();
        Ok(page(values, limit, start.order, |entry: &EntryRecord| entry.id.0))
    }

    async fn task(&self, id: TaskId, _context: &Context) -> DurableResult<Option<TaskRecord>> {
        Ok(self.open()?.state.tasks.get(&id).cloned())
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        let inner = self.open()?;
        let state = &inner.state;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let ids = match query.status {
            None => Some(&state.task_ids),
            Some(status) => state.task_ids_by_status.get(&status),
        };
        let ids = ids.map(Vec::as_slice).unwrap_or_default();
        let mut values = Vec::new();
        for index in scan_indexes(ids, start.order == ScanOrder::Ascending, start.after) {
            if values.len() > limit {
                break;
            }
            let value = &state.tasks[&TaskId(ids[index])];
            let skip = query.conversation_id.is_some_and(|id| value.conversation_id != id)
                || query.kind.as_ref().is_some_and(|kind| &value.kind != kind)
                || query.abort_requested.is_some_and(|abort| value.abort_requested != abort)
                || query.background.is_some_and(|background| value.background != background);
            if !skip {
                values.push(value.clone());
            }
        }
        Ok(page(values, limit, start.order, |value: &TaskRecord| value.id.0))
    }

    async fn submission(&self, id: SubmissionId, _context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        Ok(self.open()?.state.submissions.get(&id).cloned())
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        let inner = self.open()?;
        let state = &inner.state;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let ids = match query.status {
            None => Some(&state.submission_ids),
            Some(status) => state.submission_ids_by_status.get(&status),
        };
        let ids = ids.map(Vec::as_slice).unwrap_or_default();
        let mut values = Vec::new();
        for index in scan_indexes(ids, start.order == ScanOrder::Ascending, start.after) {
            if values.len() > limit {
                break;
            }
            let value = &state.submissions[&SubmissionId(ids[index])];
            if query.conversation_id.is_none_or(|id| value.conversation_id == id) {
                values.push(value.clone());
            }
        }
        Ok(page(values, limit, start.order, |value: &SubmissionRecord| value.id.0))
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        _context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        let inner = self.open()?;
        let id = inner.state.submission_ids_by_request.get(&conversation_id).and_then(|ids| ids.get(request_id));
        Ok(id.map(|id| inner.state.submissions[id].clone()))
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        _context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        let inner = self.open()?;
        let state = &inner.state;
        let Some(index) = state.document_addresses.get(address) else { return Ok(None) };
        if at == DocumentPoint::Current {
            return Ok(index.current_id.map(|id| state.documents[&id].record.clone()));
        }
        Ok(index
            .ids
            .iter()
            .map(|id| &state.documents[&DocumentId(*id)].record)
            .find(|record| is_alive_at(record, at))
            .cloned())
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        _context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.document_sync(id, at)
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        _context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        let inner = self.open()?;
        let state = &inner.state;
        let ids = state.document_ids_by_scope.get(&query.scope).map(Vec::as_slice).unwrap_or_default();
        let first = cursor_id(cursor)?.map_or(0, |after| upper_bound(ids, after));
        let mut values = Vec::new();
        for id in &ids[first..] {
            if values.len() > limit {
                break;
            }
            let record = &state.documents[&DocumentId(*id)].record;
            if query.kind.as_ref().is_some_and(|kind| &record.kind != kind) {
                continue;
            }
            if is_alive_at(record, query.at) {
                values.push(record.clone());
            }
        }
        Ok(page(values, limit, ScanOrder::Ascending, |record: &DocumentRecord| record.id.0))
    }

    async fn close(&self, _context: &Context) -> DurableResult<()> {
        self.close_sync();
        Ok(())
    }
}
