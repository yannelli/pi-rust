//! Tables and secondary indexes of `MemoryStorage`, and the reads that walk them.

use std::collections::HashMap;

use pi_chord::Json;
use pi_chord::delta::{Op, apply_immutable_batches};

use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::types::{
    ConversationRecord, DocumentAddress, DocumentContent, DocumentPoint, DocumentRecord, DocumentScope, EntryRecord,
    StoredDocument, SubmissionRecord, SubmissionStatus, TaskRecord, TaskStatus,
};

/// Table that owns one ID of the global namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableName {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl TableName {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::Entry => "entry",
            Self::Task => "task",
            Self::Submission => "submission",
            Self::Document => "document",
        }
    }
}

/// One stored base or delta and the commit that wrote it.
#[derive(Clone, Debug)]
pub(crate) struct Revision {
    pub seq: Seq,
    pub content: DocumentContent,
}

#[derive(Clone, Debug)]
pub(crate) struct StoredDocumentState {
    pub record: DocumentRecord,
    pub revisions: Vec<Revision>,
}

/// Every incarnation ever created at one logical address, and the current one.
#[derive(Clone, Debug, Default)]
pub(crate) struct AddressIndex {
    pub ids: Vec<u64>,
    pub current_id: Option<DocumentId>,
}

#[derive(Default)]
pub(crate) struct State {
    pub record_types: HashMap<u64, TableName>,
    pub conversations: HashMap<ConversationId, ConversationRecord>,
    pub conversation_ids: Vec<u64>,
    pub conversation_ids_by_owner_conversation: HashMap<ConversationId, Vec<u64>>,
    pub conversation_ids_by_owner_task: HashMap<TaskId, Vec<u64>>,
    pub entries: HashMap<EntryId, EntryRecord>,
    pub entry_ids: HashMap<ConversationId, Vec<u64>>,
    pub head_entry_ids: HashMap<ConversationId, Vec<u64>>,
    pub entry_commit_seqs: HashMap<EntryId, Seq>,
    pub tasks: HashMap<TaskId, TaskRecord>,
    pub task_ids: Vec<u64>,
    pub task_ids_by_status: HashMap<TaskStatus, Vec<u64>>,
    pub submissions: HashMap<SubmissionId, SubmissionRecord>,
    pub submission_ids: Vec<u64>,
    pub submission_ids_by_status: HashMap<SubmissionStatus, Vec<u64>>,
    pub submission_ids_by_request: HashMap<ConversationId, HashMap<String, SubmissionId>>,
    pub documents: HashMap<DocumentId, StoredDocumentState>,
    pub document_addresses: HashMap<DocumentAddress, AddressIndex>,
    pub document_ids_by_scope: HashMap<DocumentScope, Vec<u64>>,
}

/// First index whose ID is not below `target`.
pub(crate) fn lower_bound(ids: &[u64], target: i64) -> usize {
    ids.partition_point(|&id| (id as i64) < target)
}

/// First index whose ID is above `target`.
pub(crate) fn upper_bound(ids: &[u64], target: i64) -> usize {
    ids.partition_point(|&id| (id as i64) <= target)
}

pub(crate) fn insert_sorted(ids: &mut Vec<u64>, id: u64) {
    match ids.last() {
        None => ids.push(id),
        Some(&last) if last < id => ids.push(id),
        Some(_) => {
            let index = ids.partition_point(|&value| value < id);
            if ids.get(index) != Some(&id) {
                ids.insert(index, id);
            }
        }
    }
}

pub(crate) fn remove_sorted(ids: &mut Vec<u64>, id: u64) {
    if let Ok(index) = ids.binary_search(&id) {
        ids.remove(index);
    }
}

/// Indexes of sorted `ids` in scan order, after the cursor's ID when there is one.
pub(crate) fn scan_indexes(ids: &[u64], ascending: bool, after: Option<i64>) -> Box<dyn Iterator<Item = usize>> {
    if ascending {
        let first = after.map_or(0, |after| upper_bound(ids, after));
        Box::new(first..ids.len())
    } else {
        let end = after.map_or(ids.len(), |after| lower_bound(ids, after));
        Box::new((0..end).rev())
    }
}

pub(crate) fn is_alive_at(record: &DocumentRecord, at: DocumentPoint) -> bool {
    match at {
        DocumentPoint::Current => record.retired_at.is_none(),
        DocumentPoint::At(at) => record.created_at <= at && record.retired_at.is_none_or(|retired| at < retired),
    }
}

/// Whether the incarnation keeps only its current content: every document except a rewindable conversation one.
pub(crate) fn is_current_only(record: &DocumentRecord) -> bool {
    !matches!(record.scope, DocumentScope::Conversation { .. })
        || record.history != Some(crate::types::DocumentHistory::Rewindable)
}

pub(crate) fn unknown_conversation(id: ConversationId) -> DurableError {
    DurableError::storage_request(format!("Unknown conversation: {id}"))
}

/// Replay `revisions[base_index]` and its delta tail into one value.
pub(crate) fn replay(id: DocumentId, revisions: &[&DocumentContent]) -> DurableResult<(u32, Json, u32)> {
    let base_index = revisions.iter().rposition(|content| matches!(content, DocumentContent::Base { .. }));
    let Some(base_index) = base_index else {
        return Err(DurableError::storage(format!("Document {id} is missing a required base")));
    };
    let DocumentContent::Base { version, value } = revisions[base_index] else { unreachable!() };
    let mut batches: Vec<&[Op]> = Vec::with_capacity(revisions.len() - base_index - 1);
    for content in &revisions[base_index + 1..] {
        match content {
            DocumentContent::Delta { version: delta_version, ops } if delta_version == version => batches.push(ops),
            _ => {
                return Err(DurableError::storage(format!(
                    "Document {id} crosses a stored version boundary without a base"
                )));
            }
        }
    }
    let deltas = batches.len() as u32;
    let value = if batches.is_empty() { value.clone() } else { apply_immutable_batches(Some(value), batches)? };
    Ok((*version, value, deltas))
}

impl State {
    pub(crate) fn materialize(&self, id: DocumentId, at: DocumentPoint) -> DurableResult<Option<StoredDocument>> {
        let Some(stored) = self.documents.get(&id) else { return Ok(None) };
        if at != DocumentPoint::Current && is_current_only(&stored.record) {
            return Err(DurableError::storage_request(format!("Document {id} does not retain historical content")));
        }
        if !is_alive_at(&stored.record, at) {
            return Ok(None);
        }
        let revisions: Vec<&DocumentContent> = stored
            .revisions
            .iter()
            .filter(|revision| match at {
                DocumentPoint::Current => true,
                DocumentPoint::At(at) => revision.seq <= at,
            })
            .map(|revision| &revision.content)
            .collect();
        let (version, value, deltas_since_base) = replay(id, &revisions)?;
        Ok(Some(StoredDocument { record: stored.record.clone(), version, value, deltas_since_base }))
    }

    /// Visible entries newest first through every ancestor cap, stopping after `stop_after` entries.
    pub(crate) fn visible_entries(
        &self,
        conversation_id: ConversationId,
        min: i64,
        max: i64,
        stop_after: usize,
    ) -> DurableResult<Vec<&EntryRecord>> {
        if !self.conversations.contains_key(&conversation_id) {
            return Err(unknown_conversation(conversation_id));
        }
        let mut found = Vec::new();
        let mut current = conversation_id;
        let mut upper = max;
        loop {
            let ids = self.entry_ids.get(&current).map(Vec::as_slice).unwrap_or_default();
            for index in (0..upper_bound(ids, upper)).rev() {
                let id = ids[index];
                if (id as i64) < min {
                    break;
                }
                found.push(&self.entries[&EntryId(id)]);
                if found.len() >= stop_after {
                    return Ok(found);
                }
            }
            let Some(parent) = self.conversations[&current].parent else { break };
            upper = upper.min(parent.at.0 as i64);
            if upper < min {
                break;
            }
            current = parent.conversation_id;
        }
        Ok(found)
    }

    /// Visible entries oldest first: the fork chain's segments from the root conversation forward.
    pub(crate) fn visible_entries_ascending(
        &self,
        conversation_id: ConversationId,
        min: i64,
        max: i64,
        stop_after: usize,
    ) -> DurableResult<Vec<&EntryRecord>> {
        if !self.conversations.contains_key(&conversation_id) {
            return Err(unknown_conversation(conversation_id));
        }
        let mut segments = Vec::new();
        let mut current = conversation_id;
        let mut upper = max;
        loop {
            segments.push((current, upper));
            let Some(parent) = self.conversations[&current].parent else { break };
            upper = upper.min(parent.at.0 as i64);
            if upper < min {
                break;
            }
            current = parent.conversation_id;
        }
        let mut found = Vec::new();
        for (conversation, upper) in segments.into_iter().rev() {
            let ids = self.entry_ids.get(&conversation).map(Vec::as_slice).unwrap_or_default();
            for &id in &ids[lower_bound(ids, min)..] {
                if id as i64 > upper {
                    break;
                }
                found.push(&self.entries[&EntryId(id)]);
                if found.len() >= stop_after {
                    return Ok(found);
                }
            }
        }
        Ok(found)
    }

    pub(crate) fn latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> DurableResult<Option<&EntryRecord>> {
        if !self.conversations.contains_key(&conversation_id) {
            return Err(unknown_conversation(conversation_id));
        }
        let mut current = conversation_id;
        let mut upper = at_or_before.map_or(i64::MAX, |id| id.0 as i64);
        loop {
            let ids = self.head_entry_ids.get(&current).map(Vec::as_slice).unwrap_or_default();
            let index = upper_bound(ids, upper);
            if index > 0 {
                return Ok(Some(&self.entries[&EntryId(ids[index - 1])]));
            }
            let Some(parent) = self.conversations[&current].parent else { return Ok(None) };
            upper = upper.min(parent.at.0 as i64);
            current = parent.conversation_id;
        }
    }
}
