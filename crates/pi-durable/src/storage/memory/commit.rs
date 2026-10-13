//! Validation and application of one `MemoryStorage` commit.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use super::state::{
    AddressIndex, Revision, State, StoredDocumentState, TableName, insert_sorted, is_current_only, remove_sorted,
};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{DocumentId, Seq};
use crate::types::{DocumentAddress, DocumentContent, DocumentCreate, DocumentPoint, DocumentScope, StorageWrite};

/// Content and lifecycle commands of one document in one batch.
#[derive(Clone, Debug, Default)]
pub(crate) struct DocumentAction {
    pub create: Option<DocumentCreate>,
    pub content: Option<DocumentContent>,
    pub retire: bool,
}

/// A fully validated, detached state mutation whose application can no longer fail.
#[derive(Debug)]
pub struct PreparedMemoryCommit {
    pub(crate) seq: Seq,
    pub(crate) writes: Vec<StorageWrite>,
    pub(crate) actions: Vec<(DocumentId, DocumentAction)>,
    pub(crate) applied: AtomicBool,
}

impl PreparedMemoryCommit {
    pub fn seq(&self) -> Seq {
        self.seq
    }

    /// The detached writes for persistence, with every `document.copy` resolved to a `document.create`.
    pub fn writes(&self) -> &[StorageWrite] {
        &self.writes
    }

    /// Mark the commit applied; true only the first time.
    pub(crate) fn take(&self) -> bool {
        !self.applied.swap(true, Ordering::SeqCst)
    }
}

fn storage_error(message: String) -> DurableError {
    DurableError::storage(message)
}

impl State {
    /// Turn every `document.copy` into a `document.create` with the source's committed pre-batch base.
    pub(crate) fn resolve_document_copies(&self, writes: Vec<StorageWrite>) -> DurableResult<Vec<StorageWrite>> {
        if !writes.iter().any(|write| matches!(write, StorageWrite::DocumentCopy { .. })) {
            return Ok(writes);
        }
        let mut changed = HashSet::new();
        for write in &writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
                    changed.insert(record.id);
                }
                StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => {
                    changed.insert(*id);
                }
                _ => {}
            }
        }
        writes
            .into_iter()
            .map(|write| {
                let StorageWrite::DocumentCopy { record, source } = write else { return Ok(write) };
                let content = self
                    .copy_source(&record, source.id, source.at, &changed)
                    .map_err(|cause| storage_error(format!("Document copy {} was rejected: {cause}", record.id)))?;
                Ok(StorageWrite::DocumentCreate { record, content })
            })
            .collect()
    }

    fn copy_source(
        &self,
        record: &DocumentCreate,
        source: DocumentId,
        at: DocumentPoint,
        changed: &HashSet<DocumentId>,
    ) -> DurableResult<DocumentContent> {
        if changed.contains(&source) {
            return Err(storage_error(format!("Fork source document {source} is changed in the copy batch")));
        }
        let Some(stored) = self.materialize(source, at)? else {
            return Err(storage_error(format!("Fork source document {source} cannot be read")));
        };
        let matches = matches!(stored.record.scope, DocumentScope::Conversation { .. })
            && matches!(record.scope, DocumentScope::Conversation { .. })
            && stored.record.kind == record.kind
            && stored.record.key == record.key
            && stored.record.history == record.history
            && stored.record.fork == record.fork;
        if !matches {
            return Err(storage_error(format!("Fork source document {source} does not match the copied record")));
        }
        Ok(DocumentContent::Base { version: stored.version, value: stored.value })
    }

    pub(crate) fn check_global_ids(&self, writes: &[StorageWrite]) -> DurableResult<()> {
        let mut claimed: HashMap<u64, TableName> = HashMap::new();
        for write in writes {
            let (table, id) = match write {
                StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => continue,
                StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
                    (TableName::Document, record.id.0)
                }
                StorageWrite::Conversation { value } => (TableName::Conversation, value.id.0),
                StorageWrite::Entry { value } => (TableName::Entry, value.id.0),
                StorageWrite::Task { value } => (TableName::Task, value.id.0),
                StorageWrite::Submission { value } => (TableName::Submission, value.id.0),
            };
            let existing = self.record_types.get(&id).copied();
            let earlier = claimed.get(&id).copied();
            if matches!(table, TableName::Conversation | TableName::Entry | TableName::Document) {
                if let Some(existing) = existing {
                    return Err(storage_error(format!("ID {id} already belongs to {}", existing.as_str())));
                }
                if earlier.is_some() {
                    return Err(storage_error(format!("ID {id} is written more than once")));
                }
            } else {
                if let Some(existing) = existing.filter(|existing| *existing != table) {
                    return Err(storage_error(format!("ID {id} already belongs to {}", existing.as_str())));
                }
                if earlier.is_some_and(|earlier| earlier != table) {
                    return Err(storage_error(format!("ID {id} is written as two record types")));
                }
            }
            claimed.insert(id, table);
        }
        Ok(())
    }

    pub(crate) fn prepare_document_actions(
        writes: &[StorageWrite],
    ) -> DurableResult<Vec<(DocumentId, DocumentAction)>> {
        let mut actions: Vec<(DocumentId, DocumentAction)> = Vec::new();
        let mut positions: HashMap<DocumentId, usize> = HashMap::new();
        for write in writes {
            let id = match write {
                StorageWrite::DocumentCreate { record, .. } => record.id,
                StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => *id,
                _ => continue,
            };
            let position = *positions.entry(id).or_insert_with(|| {
                actions.push((id, DocumentAction::default()));
                actions.len() - 1
            });
            let action = &mut actions[position].1;
            match write {
                StorageWrite::DocumentCreate { record, content } => {
                    if action.create.is_some() || action.content.is_some() {
                        return Err(storage_error(format!("Document {id} has more than one content command")));
                    }
                    action.create = Some(record.clone());
                    action.content = Some(content.clone());
                }
                StorageWrite::DocumentChange { content, .. } => {
                    if action.content.is_some() {
                        return Err(storage_error(format!("Document {id} has more than one content command")));
                    }
                    action.content = Some(content.clone());
                }
                StorageWrite::DocumentRetire { .. } => {
                    if action.retire {
                        return Err(storage_error(format!("Document {id} is retired more than once")));
                    }
                    action.retire = true;
                }
                _ => unreachable!(),
            }
        }
        Ok(actions)
    }

    pub(crate) fn check_document_actions(&self, actions: &[(DocumentId, DocumentAction)]) -> DurableResult<()> {
        let mut live_counts: HashMap<DocumentAddress, i64> = HashMap::new();
        for (id, action) in actions {
            let existing = self.documents.get(id);
            let address = match (&action.create, existing) {
                (None, None) => return Err(storage_error(format!("Unknown document: {id}"))),
                (Some(_), Some(_)) => return Err(storage_error(format!("Document {id} already exists"))),
                (Some(create), None) => create.address(),
                (None, Some(existing)) => existing.record.address(),
            };
            if existing.is_some_and(|existing| existing.record.retired_at.is_some()) {
                return Err(storage_error(format!("Document {id} is retired")));
            }
            if let Some(DocumentContent::Delta { version, .. }) = &action.content {
                let Some(last) = existing.and_then(|existing| existing.revisions.last()) else {
                    return Err(storage_error(format!("Document {id} delta has no base")));
                };
                if last.content.version() != *version {
                    return Err(storage_error(format!("Document {id} version transition requires a base")));
                }
            }
            let current_id = self.document_addresses.get(&address).and_then(|index| index.current_id);
            let live = live_counts.entry(address).or_insert(i64::from(current_id.is_some()));
            if action.retire && current_id == Some(*id) {
                *live -= 1;
            }
            if action.create.is_some() && !action.retire {
                *live += 1;
            }
        }
        if live_counts.values().any(|live| *live > 1) {
            return Err(storage_error("Document address already has a current incarnation".into()));
        }
        Ok(())
    }

    /// Apply validated writes; returns the largest record ID written.
    pub(crate) fn apply_writes(&mut self, writes: &[StorageWrite], seq: Seq) -> u64 {
        let mut max_id = 0;
        for write in writes {
            match write {
                StorageWrite::Conversation { value } => {
                    self.record_types.insert(value.id.0, TableName::Conversation);
                    self.conversations.insert(value.id, *value);
                    insert_sorted(&mut self.conversation_ids, value.id.0);
                    if let Some(owner) = value.owner {
                        let by_conversation = self.conversation_ids_by_owner_conversation.entry(owner.conversation_id);
                        insert_sorted(by_conversation.or_default(), value.id.0);
                        insert_sorted(
                            self.conversation_ids_by_owner_task.entry(owner.task_id).or_default(),
                            value.id.0,
                        );
                    }
                    max_id = max_id.max(value.id.0);
                }
                StorageWrite::Entry { value } => {
                    self.record_types.insert(value.id.0, TableName::Entry);
                    self.entries.insert(value.id, value.clone());
                    self.entry_commit_seqs.insert(value.id, seq);
                    insert_sorted(self.entry_ids.entry(value.conversation_id).or_default(), value.id.0);
                    if value.head.is_some() {
                        insert_sorted(self.head_entry_ids.entry(value.conversation_id).or_default(), value.id.0);
                    }
                    max_id = max_id.max(value.id.0);
                }
                StorageWrite::Task { value } => {
                    self.record_types.insert(value.id.0, TableName::Task);
                    let status = value.status();
                    match self.tasks.get(&value.id).map(|stored| stored.status()) {
                        None => {
                            insert_sorted(&mut self.task_ids, value.id.0);
                            insert_sorted(self.task_ids_by_status.entry(status).or_default(), value.id.0);
                        }
                        Some(old) if old != status => {
                            remove_sorted(self.task_ids_by_status.entry(old).or_default(), value.id.0);
                            insert_sorted(self.task_ids_by_status.entry(status).or_default(), value.id.0);
                        }
                        Some(_) => {}
                    }
                    self.tasks.insert(value.id, value.clone());
                    max_id = max_id.max(value.id.0);
                }
                StorageWrite::Submission { value } => {
                    self.record_types.insert(value.id.0, TableName::Submission);
                    let old = self.submissions.get(&value.id).cloned();
                    match &old {
                        None => {
                            insert_sorted(&mut self.submission_ids, value.id.0);
                            insert_sorted(self.submission_ids_by_status.entry(value.status).or_default(), value.id.0);
                        }
                        Some(old) if old.status != value.status => {
                            remove_sorted(self.submission_ids_by_status.entry(old.status).or_default(), value.id.0);
                            insert_sorted(self.submission_ids_by_status.entry(value.status).or_default(), value.id.0);
                        }
                        Some(_) => {}
                    }
                    if let Some(old) = &old
                        && let Some(request_id) = &old.request_id
                        && let Some(requests) = self.submission_ids_by_request.get_mut(&old.conversation_id)
                        && requests.get(request_id) == Some(&value.id)
                    {
                        requests.remove(request_id);
                        if requests.is_empty() {
                            self.submission_ids_by_request.remove(&old.conversation_id);
                        }
                    }
                    self.submissions.insert(value.id, value.clone());
                    if let Some(request_id) = &value.request_id {
                        let requests = self.submission_ids_by_request.entry(value.conversation_id).or_default();
                        requests.insert(request_id.clone(), value.id);
                    }
                    max_id = max_id.max(value.id.0);
                }
                StorageWrite::DocumentCreate { .. }
                | StorageWrite::DocumentCopy { .. }
                | StorageWrite::DocumentChange { .. }
                | StorageWrite::DocumentRetire { .. } => {}
            }
        }
        max_id
    }

    /// Apply validated document commands; returns the largest document ID created.
    pub(crate) fn apply_document_actions(&mut self, actions: &[(DocumentId, DocumentAction)], seq: Seq) -> u64 {
        let mut max_id = 0;
        for (id, action) in actions {
            if let Some(create) = &action.create {
                let mut record = create.clone().into_record(seq);
                if action.retire {
                    record.retired_at = Some(seq);
                }
                let content = action.content.clone().expect("a create carries its base");
                let address = record.address();
                let scope = record.scope;
                self.documents.insert(*id, StoredDocumentState { record, revisions: vec![Revision { seq, content }] });
                self.record_types.insert(id.0, TableName::Document);
                insert_sorted(&mut self.document_addresses.entry(address).or_default().ids, id.0);
                insert_sorted(self.document_ids_by_scope.entry(scope).or_default(), id.0);
                max_id = max_id.max(id.0);
            } else if let Some(content) = &action.content {
                let stored = self.documents.get_mut(id).expect("a checked document exists");
                let revision = Revision { seq, content: content.clone() };
                if matches!(content, DocumentContent::Base { .. }) && is_current_only(&stored.record) {
                    stored.revisions = vec![revision];
                } else {
                    stored.revisions.push(revision);
                }
            }
            let stored = self.documents.get_mut(id).expect("a checked document exists");
            if action.retire && action.create.is_none() {
                stored.record.retired_at = Some(seq);
            }
            if action.retire && is_current_only(&stored.record) {
                stored.revisions.clear();
            }
            if action.create.is_some() || action.retire {
                let address: &mut AddressIndex =
                    self.document_addresses.get_mut(&stored.record.address()).expect("an indexed address");
                if action.retire && address.current_id == Some(*id) {
                    address.current_id = None;
                }
                if action.create.is_some() && !action.retire {
                    address.current_id = Some(*id);
                }
            }
        }
        max_id
    }
}
