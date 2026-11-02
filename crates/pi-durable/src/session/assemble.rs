//! Transaction settlement: seal, prepare, assemble the atomic batch, and adopt it after Storage succeeds.

use std::collections::HashSet;
use std::sync::Mutex;

use super::core::{AdoptedDocuments, LoadedDocument, LoadedMeta};
use super::line::lock;
use super::plans::{DocumentPlan, PlanRecord, apply_submission_change, plan_document};
use super::tx::{TaskWriteKind, Tx, TxState};
use crate::documents::{CheckpointInfo, address_id};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, Seq, TaskId};
use crate::types::*;

const INTERNAL_SCAN_PAGE_SIZE: usize = 256;

fn abort_changes(state: &TxState) {
    for document in &state.documents {
        if let Some(change) = &document.change {
            lock(change).abort();
        }
        if let Some(prepared) = &document.prepared {
            prepared.abort();
        }
    }
}

impl Tx {
    /// Seal after callback failure: abort every change and wait for every pending operation.
    pub(crate) async fn settle_failure(&self) {
        {
            let mut state = lock(&self.0.state);
            state.sealed = true;
            abort_changes(&state);
        }
        self.0.pending.drained().await;
    }

    /// Seal after callback success, prepare every open change, and assemble the atomic batch.
    pub(crate) async fn settle_success(&self) -> DurableResult<Vec<StorageWrite>> {
        let pending = {
            let mut state = lock(&self.0.state);
            state.sealed = true;
            let pending = self.0.pending.count() > 0;
            if pending {
                abort_changes(&state);
            }
            pending
        };
        if pending {
            self.0.pending.drained().await;
            return Err(DurableError::error("Session commit callback settled before its pending Tx operations"));
        }
        {
            let mut state = lock(&self.0.state);
            // Prepare every open change; this revokes every draft.
            let mut failure = None;
            for document in &mut state.documents {
                if let Some(change) = &document.change {
                    match lock(change).prepare() {
                        Ok(prepared) => document.prepared = Some(prepared),
                        Err(error) => {
                            failure = Some(DurableError::from(error));
                            break;
                        }
                    }
                }
            }
            if let Some(error) = failure {
                abort_changes(&state);
                return Err(error);
            }
        }
        match self.assemble().await {
            Ok(writes) => Ok(writes),
            Err(error) => {
                abort_changes(&lock(&self.0.state));
                Err(error)
            }
        }
    }

    /// Abort every prepared change after Storage failure or when no write is required.
    pub(crate) fn discard(&self) {
        abort_changes(&lock(&self.0.state));
    }

    /// Drop what the settled transaction staged. A draft holds its transaction, so the documents staged here would
    /// otherwise keep the transaction, and through it the Session, alive.
    pub(crate) fn release(&self) {
        let staged = {
            let mut state = lock(&self.0.state);
            (std::mem::take(&mut state.documents), std::mem::take(&mut state.plans), std::mem::take(&mut state.tasks))
        };
        drop(staged);
    }

    async fn assemble(&self) -> DurableResult<Vec<StorageWrite>> {
        let tx = &self.0;
        let mut plans: Vec<DocumentPlan> = lock(&tx.state).documents.iter().filter_map(plan_document).collect();
        self.reject_fork_source_writes(&plans)?;
        self.validate_owners().await?;
        let replaced: Vec<(TaskId, ConversationId)> = {
            let state = lock(&tx.state);
            let writes = state.task_order.iter().filter_map(|id| state.tasks.get(id)?.write.as_ref());
            writes.filter(|w| w.0 == TaskWriteKind::Replace).map(|w| (w.1.id, w.1.conversation_id)).collect()
        };
        for (id, conversation_id) in replaced {
            let Some(committed) = tx.committed_task(id).await? else {
                return Err(DurableError::error(format!("Task {id} does not exist")));
            };
            if committed.is_terminal() {
                return Err(DurableError::error(format!("Task {id} is already terminal")));
            }
            if committed.conversation_id != conversation_id {
                return Err(DurableError::error(format!("Task {id} cannot change conversations")));
            }
        }
        self.retire_terminal_task_documents(&mut plans).await?;
        // Resolve publication ownership before Storage admission so adoption remains synchronous.
        for plan in &mut plans {
            if !plan.publishes() {
                continue;
            }
            match plan.record.scope() {
                DocumentScope::Conversation { conversation_id } => plan.conversation_id = Some(conversation_id),
                DocumentScope::Task { task_id } => {
                    let known = lock(&tx.state).task_entry(task_id).publication_conversation_id;
                    let conversation_id = match known {
                        Some(id) => Some(id),
                        None => tx.current_task(task_id).await?.map(|task| task.conversation_id),
                    };
                    lock(&tx.state).task_entry(task_id).publication_conversation_id = conversation_id;
                    plan.conversation_id = conversation_id;
                }
                DocumentScope::Session => {}
            }
        }
        let changes = std::mem::take(&mut lock(&tx.state).submission_changes);
        for (id, change) in changes {
            let staged = lock(&tx.state).submissions.iter().find(|record| record.id == id).cloned();
            let current = match staged {
                Some(record) => record,
                None => {
                    let stored = tx.core.storage.submission(id, &tx.context).await?;
                    stored.ok_or_else(|| DurableError::error(format!("Submission {id} does not exist")))?
                }
            };
            if let Some(next) = apply_submission_change(&current, change)? {
                let mut state = lock(&tx.state);
                match state.submissions.iter_mut().find(|record| record.id == id) {
                    Some(slot) => *slot = next,
                    None => state.submissions.push(next),
                }
            }
        }
        let mut state = lock(&tx.state);
        let mut writes = std::mem::take(&mut state.writes);
        writes.extend(state.submissions.iter().map(|value| StorageWrite::Submission { value: value.clone() }));
        for id in &state.task_order {
            if let Some((_, record)) = state.tasks.get(id).and_then(|task| task.write.as_ref()) {
                writes.push(StorageWrite::Task { value: record.clone() });
            }
        }
        for plan in &mut plans {
            // Checkpoint predicates run last, after every validation.
            if let (Some(StorageWrite::DocumentChange { id, content: DocumentContent::Delta { .. } }), Some(change)) =
                (&plan.content, &plan.change)
                && let (Some(loaded), Some(definition)) = (&change.loaded, &change.definition)
            {
                let info = CheckpointInfo { deltas_since_base: lock(&loaded.meta).deltas_since_base };
                let prepared = &change.prepared;
                if definition.checkpoint_when(&prepared.value, &prepared.ops, info)? {
                    let content = DocumentContent::Base { version: change.version, value: prepared.value.clone() };
                    plan.content = Some(StorageWrite::DocumentChange { id: *id, content });
                }
            }
            if let Some(content) = &plan.content {
                writes.push(content.clone());
            }
            if plan.retire {
                writes.push(StorageWrite::DocumentRetire { id: plan.record.id() });
            }
        }
        state.plans = plans;
        Ok(writes)
    }

    /// Terminal settlement retires every task document, including ones created by this transaction.
    async fn retire_terminal_task_documents(&self, plans: &mut Vec<DocumentPlan>) -> DurableResult<()> {
        let tx = &self.0;
        let terminal: Vec<(TaskId, bool)> = {
            let state = lock(&tx.state);
            let writes = state.task_order.iter().filter_map(|id| state.tasks.get(id)?.write.as_ref());
            writes.filter(|w| w.1.is_terminal()).map(|w| (w.1.id, w.0 == TaskWriteKind::Create)).collect()
        };
        if terminal.is_empty() {
            return Ok(());
        }
        let mut retiring = HashSet::new();
        for plan in plans.iter_mut() {
            if let DocumentScope::Task { task_id } = plan.record.scope()
                && terminal.iter().any(|(id, _)| *id == task_id)
            {
                plan.retire = true;
                retiring.insert(plan.record.id());
            }
        }
        for (task_id, created) in terminal {
            if created {
                continue;
            }
            let mut cursor: Option<Cursor> = None;
            loop {
                let scope = DocumentScope::Task { task_id };
                let query = DocumentQuery { scope, at: DocumentPoint::Current, kind: None };
                let storage = &tx.core.storage;
                let limit = INTERNAL_SCAN_PAGE_SIZE;
                let page = storage.scan_documents(&query, limit, cursor.as_ref(), &tx.context).await?;
                for record in page.items {
                    if retiring.insert(record.id) {
                        plans.push(DocumentPlan::retirement(address_id(&record.address()), record));
                    }
                }
                cursor = page.next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// New owned work needs a live owner, judged on its final candidate: not completing, terminal, or abort-marked.
    async fn validate_owners(&self) -> DurableResult<()> {
        let owners: Vec<(&str, TaskId)> = {
            let state = lock(&self.0.state);
            let conversations = state.writes.iter().filter_map(|write| match write {
                StorageWrite::Conversation { value } => {
                    value.owner.map(|owner| ("Conversation owner task", owner.task_id))
                }
                _ => None,
            });
            let tasks = state.task_order.iter().filter_map(|id| match state.tasks.get(id)?.write.as_ref()? {
                (TaskWriteKind::Create, record) => record.owner.map(|owner| ("Task owner", owner)),
                _ => None,
            });
            conversations.chain(tasks).collect()
        };
        for (what, task_id) in owners {
            let Some(task) = self.0.current_task(task_id).await? else {
                return Err(DurableError::error(format!("{what} {task_id} does not exist")));
            };
            let status = task.status();
            if matches!(status, TaskStatus::Terminal | TaskStatus::Completing) {
                return Err(DurableError::error(format!("{what} {task_id} is {}", status.as_str())));
            }
            if task.abort_requested {
                return Err(DurableError::error(format!("{what} {task_id} is abort-marked")));
            }
        }
        Ok(())
    }

    fn reject_fork_source_writes(&self, plans: &[DocumentPlan]) -> DurableResult<()> {
        let state = lock(&self.0.state);
        for plan in plans {
            if plan.content.is_none() && !plan.retire {
                continue;
            }
            let id = plan.record.id();
            if state.fork_source_document_ids.contains(&id) {
                let message = format!("Cannot change fork source document {id} in the fork transaction");
                return Err(DurableError::error(message));
            }
            if let DocumentScope::Conversation { conversation_id } = plan.record.scope()
                && state.fork_source_conversation_ids.contains(&conversation_id)
                && plan.record.fork() == Some(DocumentFork::Current)
            {
                return Err(DurableError::error(format!(
                    "Cannot fork conversation {conversation_id} while changing its current-policy documents"
                )));
            }
        }
        Ok(())
    }

    /// Adopt every prepared change by pointer swap after Storage success and describe the publication.
    pub(crate) fn adopt(&self, seq: Seq) -> DurableResult<AdoptedDocuments> {
        let core = &self.0.core;
        let plans = std::mem::take(&mut lock(&self.0.state).plans);
        let mut publications = Vec::new();
        // Only when the commit stored held changes: then what Storage got differs from what memory adopted.
        let mut stored: Option<Vec<DocumentCommitChange>> = None;
        for plan in plans {
            let committed = matches!(plan.record, PlanRecord::Committed(_));
            let mut record = match &plan.record {
                PlanRecord::Committed(record) => record.clone(),
                PlanRecord::New(create) => create.clone().into_record(seq),
            };
            if plan.retire {
                record.retired_at = Some(seq);
            }
            if let Some(change) = &plan.change {
                // A new incarnation is adopted unless it retires in the same commit; a loaded one only when it changed.
                let adopt = match &change.loaded {
                    None => !plan.retire,
                    Some(_) => !change.prepared.ops.is_empty(),
                };
                if adopt {
                    lock(&change.tracker).adopt(&change.prepared)?;
                } else {
                    change.prepared.abort();
                }
                if let Some(loaded) = &change.loaded {
                    {
                        let mut meta = lock(&loaded.meta);
                        meta.stored_version = meta.stored_version.max(change.version);
                        if let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content {
                            match content {
                                DocumentContent::Base { .. } => meta.deltas_since_base = 0,
                                DocumentContent::Delta { .. } => meta.deltas_since_base += 1,
                            }
                        }
                    }
                    core.touch_document(&plan.address_id);
                } else if !plan.retire {
                    core.install(LoadedDocument {
                        address_id: plan.address_id.clone(),
                        record: record.clone(),
                        value_version: change.version,
                        tracker: change.tracker.clone(),
                        meta: Mutex::new(LoadedMeta { stored_version: change.version, deltas_since_base: 0 }),
                    });
                }
            }
            if plan.retire && committed {
                core.evict(&plan.address_id, record.id);
            }
            let Some(publication) = describe(&plan, record) else { continue };
            let Some((held_ops, observed)) = plan.held else {
                if let Some(stored) = &mut stored {
                    stored.push(publication.clone());
                }
                publications.push(publication);
                continue;
            };
            let stored = stored.get_or_insert_with(|| publications.clone());
            // Only a document change of a loaded incarnation carries held operations.
            let held = match &publication {
                DocumentCommitChange::Change { record, conversation_id, version, value, .. } => {
                    let (record, conversation_id, version, value) =
                        (record.clone(), *conversation_id, *version, value.clone());
                    DocumentCommitChange::Change { record, conversation_id, version, value, ops: held_ops }
                }
                other => other.clone(),
            };
            stored.push(held);
            if observed {
                publications.push(publication);
            }
        }
        Ok(AdoptedDocuments { observed: publications, stored })
    }
}

/// The publication of one adopted plan, if it publishes, with `record` as committed.
fn describe(plan: &DocumentPlan, record: DocumentRecord) -> Option<DocumentCommitChange> {
    let conversation_id = plan.conversation_id;
    if plan.retire {
        return Some(DocumentCommitChange::Retire { record, conversation_id });
    }
    if let Some(StorageWrite::DocumentCopy { source, .. }) = &plan.content {
        let conversation_id = match record.scope {
            DocumentScope::Conversation { conversation_id } => conversation_id,
            _ => conversation_id.unwrap_or_default(),
        };
        return Some(DocumentCommitChange::Copy { record, conversation_id, source: *source });
    }
    let change = plan.change.as_ref().filter(|_| plan.publishes())?;
    let ops = if change.loaded.is_some() { change.prepared.ops.clone() } else { Vec::new() };
    let (version, value) = (change.version, change.prepared.value.clone());
    Some(DocumentCommitChange::Change { record, conversation_id, version, value, ops })
}
