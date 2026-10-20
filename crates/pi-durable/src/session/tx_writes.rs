//! Table writes of a transaction: conversations, forks, entries, tasks, and submissions.

use std::sync::Arc;

use pi_chord::{Json, is_json_value};

use super::forks::prepare_fork_document_copies;
use super::line::lock;
use super::tx::{DocumentEntry, DocumentTarget, SubmissionChange, TaskWriteKind, Tx, TxFuture};
use crate::documents::address_id;
use crate::entries::{Entry, TypedEntry, TypedEntryDraft};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId, ROOT_CONVERSATION_ID, SubmissionId, TaskId};
use crate::tasks::{TaskOptions, TaskOwnership, TaskTemplate, TaskTrace};
use crate::types::*;

impl Tx {
    pub fn create_conversation(&self, ownership: ConversationOwnership) -> TxFuture<ConversationRecord> {
        let this = self.clone();
        self.write(move |_| async move { this.stage_conversation(None, ownership, None).await })
    }

    /// Internal bootstrap path for the reserved root identity.
    pub fn create_root_conversation(&self) -> TxFuture<ConversationRecord> {
        let this = self.clone();
        self.write(move |_| async move {
            this.stage_conversation(None, ConversationOwnership::Ownerless, Some(ROOT_CONVERSATION_ID)).await
        })
    }

    /// Fork at one visible entry; conversation documents are copied by their fork policy.
    pub fn fork_conversation(
        &self,
        parent_conversation_id: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> TxFuture<ConversationRecord> {
        let this = self.clone();
        let parent = ConversationParent { conversation_id: parent_conversation_id, at };
        self.write(move |_| async move { this.stage_conversation(Some(parent), ownership, None).await })
    }

    async fn stage_conversation(
        &self,
        parent: Option<ConversationParent>,
        ownership: ConversationOwnership,
        reserved: Option<ConversationId>,
    ) -> DurableResult<ConversationRecord> {
        let tx = &self.0;
        let id = match reserved {
            Some(id) => id,
            None => ConversationId(tx.mint().await?),
        };
        tx.assert_open()?;
        let owner = match ownership {
            ConversationOwnership::Ownerless => None,
            ConversationOwnership::Task { task_id } => {
                let task = tx.current_task(task_id).await?;
                tx.assert_open()?;
                let Some(task) = task else {
                    return Err(DurableError::error(format!("Conversation owner task {task_id} does not exist")));
                };
                Some(ConversationOwner { conversation_id: task.conversation_id, task_id })
            }
        };
        let record = ConversationRecord { id, parent, owner };
        let copies = match parent {
            Some(parent) => {
                let storage = tx.core.storage.as_ref();
                prepare_fork_document_copies(storage, parent.conversation_id, parent.at, id, &tx.context).await?
            }
            None => Vec::new(),
        };
        {
            let mut state = tx.open_state()?;
            for copy in copies {
                state.fork_source_document_ids.insert(copy.source.id);
                let address = copy.record.address();
                let mut entry = DocumentEntry::new(address_id(&address), address);
                entry.target = Some(DocumentTarget::ForkCopy { record: copy.record, source: copy.source });
                state.push_document(entry);
            }
            if let Some(parent) = parent {
                state.fork_source_conversation_ids.insert(parent.conversation_id);
            }
            state.created_conversation_ids.insert(id);
            state.writes.push(StorageWrite::Conversation { value: record });
        }
        tx.core.hooks.clone().conversation_created(self, &record).await?;
        tx.assert_open()?;
        Ok(record)
    }

    pub fn append_entry(&self, conversation_id: ConversationId, value: EntryDraft) -> TxFuture<EntryRecord> {
        self.write(move |tx| async move {
            tx.require_conversation(conversation_id).await?;
            tx.assert_open()?;
            let id = EntryId(tx.mint().await?);
            tx.assert_open()?;
            let head = match value.head {
                Some(DraftHead::SelfEntry) => Some(id),
                Some(DraftHead::Entry(head)) => Some(head),
                None => None,
            };
            let record = EntryRecord {
                kind: value.kind,
                model: value.model,
                data: value.data,
                edits: value.edits,
                id,
                conversation_id,
                head,
                by_task_id: tx.scope.task_id,
            };
            if !is_json_value(&record) {
                return Err(DurableError::type_error("Entry is not strict JSON"));
            }
            tx.open_state()?.writes.push(StorageWrite::Entry { value: record.clone() });
            Ok(record)
        })
    }

    /// Append an entry of a typed kind; the token supplies `kind`.
    pub fn append_typed_entry<D>(
        &self,
        token: &Entry<D>,
        conversation_id: ConversationId,
        value: TypedEntryDraft<D>,
    ) -> TxFuture<TypedEntry<D>>
    where
        D: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
    {
        let data = value.data.clone();
        match token.draft(value) {
            Ok(draft) => {
                let appended = self.append_entry(conversation_id, draft);
                self.track(async move { Ok(TypedEntry { record: appended.await?, data }) })
            }
            Err(error) => self.write(move |_| async move { Err(error) }),
        }
    }

    /// Create a pending task of `task`'s kind with a fresh ID.
    pub fn create_task(&self, task: Arc<dyn TaskTemplate>, input: Json, options: TaskOptions) -> TxFuture<TaskId> {
        self.write(move |tx| async move {
            let mut owner = None;
            if let TaskOwnership::Task(owner_id) = options.ownership {
                // Validated again against the owner's final candidate during assembly.
                let record = tx.current_task(owner_id).await?;
                tx.assert_open()?;
                let Some(record) = record else {
                    return Err(DurableError::error(format!("Task owner {owner_id} does not exist")));
                };
                if options.background {
                    return Err(DurableError::type_error("A child task cannot be background"));
                }
                if options.conversation_id.is_some_and(|id| id != record.conversation_id) {
                    let conversation = record.conversation_id;
                    return Err(DurableError::error(format!(
                        "A child task lives in its owner's conversation {conversation}"
                    )));
                }
                owner = Some(record);
            }
            let conversation_id = owner
                .as_ref()
                .map(|owner| owner.conversation_id)
                .or(options.conversation_id)
                .or(tx.scope.conversation_id)
                .ok_or_else(|| DurableError::type_error("Tx.createTask() requires options.conversationId"))?;
            tx.require_conversation(conversation_id).await?;
            tx.assert_open()?;
            let checkpoint = task.initial(&input)?;
            let id = TaskId(tx.mint().await?);
            tx.assert_open()?;
            let record = TaskRecord {
                id,
                conversation_id,
                kind: task.name().to_owned(),
                version: task.version(),
                input,
                owner: owner.map(|owner| owner.id),
                background: options.background,
                abort_requested: false,
                abort_reason: None,
                abandon_on_restart: options.abandon_on_restart.then_some(true),
                state: TaskState::Pending { checkpoint },
                memos: None,
                started_at: None,
                ended_at: None,
                restarts: None,
                trace: match options.trace {
                    TaskTrace::Inherit => {
                        let span = tx.context.telemetry();
                        span.propagation().and_then(|propagation| propagation.inject(span.as_ref()))
                    }
                    TaskTrace::Root => None,
                    TaskTrace::Carrier(carrier) => Some(carrier),
                },
                links: (!options.links.is_empty()).then_some(options.links),
            };
            tx.open_state()?.task_entry(id).write = Some((TaskWriteKind::Create, record));
            Ok(id)
        })
    }

    /// Raw submission record with a fresh ID; no admission rules apply.
    pub fn create_submission(&self, create: SubmissionCreate) -> TxFuture<SubmissionRecord> {
        self.write(move |tx| async move {
            tx.require_conversation(create.conversation_id).await?;
            tx.assert_open()?;
            let id = SubmissionId(tx.mint().await?);
            tx.assert_open()?;
            let record = create.into_record(id);
            tx.open_state()?.submissions.push(record.clone());
            Ok(record)
        })
    }

    /// Settle a queued or placed submission; resolved during assembly against the latest candidate record.
    pub fn settle_submission(&self, id: SubmissionId, settlement: SubmissionSettlement) -> DurableResult<()> {
        self.write_now()?.submission_changes.push((id, SubmissionChange::Settle(settlement)));
        Ok(())
    }

    /// Place a queued submission at `entry`; resolved during assembly like `settle_submission`.
    pub fn place_submission(&self, id: SubmissionId, entry: EntryId) -> DurableResult<()> {
        self.write_now()?.submission_changes.push((id, SubmissionChange::Place(entry)));
        Ok(())
    }

    /// Internal: replace one task record completely.
    pub fn set_task(&self, value: TaskRecord) -> DurableResult<()> {
        let now = self.0.core.now.clone();
        let mut state = self.write_now()?;
        let task = state.task_entry(value.id);
        let candidate = task.write.as_ref().map(|write| write.1.clone());
        if let Some(candidate) = &candidate {
            if candidate.is_terminal() {
                return Err(DurableError::error(format!("Task {} already has a terminal candidate", value.id)));
            }
            if candidate.conversation_id != value.conversation_id {
                return Err(DurableError::error(format!("Task {} cannot change conversations", value.id)));
            }
        }
        let kind = match task.write {
            Some((TaskWriteKind::Create, _)) => TaskWriteKind::Create,
            _ => TaskWriteKind::Replace,
        };
        task.write = Some((kind, stamp_times(value, candidate.as_ref(), now.as_ref())));
        Ok(())
    }

    /// Internal: candidate records of the tasks this transaction created or replaced so far.
    pub fn staged_tasks(&self) -> Vec<TaskRecord> {
        let state = lock(&self.0.state);
        let staged = state.task_order.iter().filter_map(|id| state.tasks.get(id)?.write.as_ref());
        staged.map(|write| write.1.clone()).collect()
    }

    /// Internal: a submission as this transaction created it, else as committed; allowed after table writes.
    pub async fn current_submission(&self, id: SubmissionId) -> DurableResult<Option<SubmissionRecord>> {
        let staged = lock(&self.0.state).submissions.iter().find(|record| record.id == id).cloned();
        match staged {
            Some(record) => Ok(Some(record)),
            None => self.0.core.storage.submission(id, &self.0.context).await,
        }
    }

    /// Internal: conversations this transaction created or forked so far.
    pub fn staged_conversations(&self) -> Vec<ConversationRecord> {
        let state = lock(&self.0.state);
        let staged = state.writes.iter().filter_map(|write| match write {
            StorageWrite::Conversation { value } => Some(*value),
            _ => None,
        });
        staged.collect()
    }
}

/// `started_at` on the first change to running, `ended_at` on the change to terminal; once set they carry over.
fn stamp_times(
    mut value: TaskRecord,
    candidate: Option<&TaskRecord>,
    now: &(dyn Fn() -> f64 + Send + Sync),
) -> TaskRecord {
    let status = value.status();
    let started = candidate.and_then(|c| c.started_at).or(value.started_at);
    let ended = candidate.and_then(|c| c.ended_at).or(value.ended_at);
    value.started_at = started.or_else(|| (status == TaskStatus::Running).then(now));
    value.ended_at = ended.or_else(|| (status == TaskStatus::Terminal).then(now));
    value
}
