//! Transaction handle and table reads of one Session commit callback (spec section 4).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};

use pi_chord::Context;
use pi_chord::delta::{Change, Prepared, Tracker};

use super::core::{LoadedDocument, SessionCore};
use super::draft::DraftCore;
use super::line::{PendingCounter, SharedResult, lock, ready_shared, spawn_shared};
use crate::documents::DocDefinition;
use crate::entries::{Entry, TypedEntry};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId, SubmissionId, TaskId};
use crate::types::*;

/// A started transaction operation. It runs whether or not it is awaited, like a JavaScript promise.
pub type TxFuture<T> = SharedResult<T>;

/// Defaults a commit binds to: the `create_task` conversation and the task attributed to appended entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransactionScope {
    pub conversation_id: Option<ConversationId>,
    /// Task whose runtime commit this is; stamped as `by_task_id` on appended entries.
    pub task_id: Option<TaskId>,
}

/// A staged submission change: a settlement, or the placement of a queued submission at its entry.
#[derive(Clone, Debug)]
pub(crate) enum SubmissionChange {
    Settle(SubmissionSettlement),
    Place(EntryId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskWriteKind {
    Create,
    Replace,
}

#[derive(Default)]
pub(crate) struct TransactionTask {
    pub committed_read: Option<SharedResult<Option<TaskRecord>>>,
    pub write: Option<(TaskWriteKind, TaskRecord)>,
    pub publication_conversation_id: Option<ConversationId>,
}

/// Storage and cache provenance of one staged document incarnation.
#[derive(Clone)]
pub(crate) enum DocumentTarget {
    Loaded(Arc<LoadedDocument>),
    Created { record: DocumentCreate, version: u32, tracker: Arc<Mutex<Tracker>> },
    ForkCopy { record: DocumentCreate, source: DocumentCopySource },
    RetireOnly(DocumentRecord),
}

/// One document incarnation acquired, created, or retired by this transaction.
pub(crate) struct DocumentEntry {
    pub address_id: String,
    pub address: DocumentAddress,
    /// Absent for definition-free fork copies and retirements found by a terminal-task scan.
    pub definition: Option<Arc<DocDefinition>>,
    pub draft: Option<SharedResult<DraftCore>>,
    pub target: Option<DocumentTarget>,
    pub change: Option<Arc<Mutex<Change>>>,
    pub prepared: Option<Prepared>,
    pub retire_on_commit: bool,
}

impl DocumentEntry {
    pub fn new(address_id: String, address: DocumentAddress) -> Self {
        Self {
            address_id,
            address,
            definition: None,
            draft: None,
            target: None,
            change: None,
            prepared: None,
            retire_on_commit: false,
        }
    }
}

#[derive(Default)]
pub(crate) struct TxState {
    pub sealed: bool,
    pub has_table_write: bool,
    /// Atomic batch; conversation and entry writes stage eagerly, task and document writes assemble later.
    pub writes: Vec<StorageWrite>,
    pub created_conversation_ids: HashSet<ConversationId>,
    pub fork_source_conversation_ids: HashSet<ConversationId>,
    pub fork_source_document_ids: HashSet<DocumentId>,
    pub tasks: HashMap<TaskId, TransactionTask>,
    pub task_order: Vec<TaskId>,
    /// Submissions created or changed by this transaction, in first-touch order.
    pub submissions: Vec<SubmissionRecord>,
    pub submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    pub plans: Vec<super::plans::DocumentPlan>,
    pub documents: Vec<DocumentEntry>,
    pub latest_document_by_address: HashMap<String, usize>,
}

impl TxState {
    pub fn task_entry(&mut self, id: TaskId) -> &mut TransactionTask {
        if !self.tasks.contains_key(&id) {
            self.task_order.push(id);
        }
        self.tasks.entry(id).or_default()
    }

    /// Push a document entry and make it the latest at its address.
    pub fn push_document(&mut self, entry: DocumentEntry) -> usize {
        let index = self.documents.len();
        self.latest_document_by_address.insert(entry.address_id.clone(), index);
        self.documents.push(entry);
        index
    }
}

pub(crate) struct TxInner {
    pub core: Arc<SessionCore>,
    pub context: Context,
    pub scope: TransactionScope,
    pub pending: Arc<PendingCounter>,
    pub state: Mutex<TxState>,
}

pub(crate) fn settled() -> DurableError {
    DurableError::error("Transaction has settled")
}

impl TxInner {
    pub fn assert_open(&self) -> DurableResult<()> {
        if lock(&self.state).sealed { Err(settled()) } else { Ok(()) }
    }

    /// The state lock, failing once sealed.
    pub fn open_state(&self) -> DurableResult<MutexGuard<'_, TxState>> {
        let state = lock(&self.state);
        if state.sealed {
            return Err(settled());
        }
        Ok(state)
    }

    pub async fn require_conversation(&self, id: ConversationId) -> DurableResult<()> {
        if lock(&self.state).created_conversation_ids.contains(&id) {
            return Ok(());
        }
        match self.core.storage.conversation(id, &self.context).await? {
            Some(_) => Ok(()),
            None => Err(DurableError::error(format!("Conversation {id} does not exist"))),
        }
    }

    /// Latest candidate task record, falling back to committed state; not a caller table read.
    pub async fn current_task(&self, id: TaskId) -> DurableResult<Option<TaskRecord>> {
        let candidate = lock(&self.state).tasks.get(&id).and_then(|task| task.write.as_ref().map(|w| w.1.clone()));
        match candidate {
            Some(record) => Ok(Some(record)),
            None => self.committed_task(id).await,
        }
    }

    pub fn committed_task(&self, id: TaskId) -> SharedResult<Option<TaskRecord>> {
        let mut state = lock(&self.state);
        let task = state.task_entry(id);
        if let Some(read) = &task.committed_read {
            return read.clone();
        }
        let storage = self.core.storage.clone();
        let context = self.context.clone();
        let read = spawn_shared(async move { storage.task(id, &context).await });
        task.committed_read = Some(read.clone());
        read
    }

    pub async fn mint(&self) -> DurableResult<u64> {
        self.core.storage.mint_id().await
    }
}

/// Transaction of one Session commit callback; a cheap handle. Every operation fails once the callback settles.
#[derive(Clone)]
pub struct Tx(pub(crate) Arc<TxInner>);

impl Tx {
    pub(crate) fn new(core: Arc<SessionCore>, context: Context, scope: TransactionScope) -> Self {
        Self(Arc::new(TxInner { core, context, scope, pending: Arc::default(), state: Mutex::default() }))
    }

    pub fn context(&self) -> &Context {
        &self.0.context
    }

    pub fn scope(&self) -> TransactionScope {
        self.0.scope
    }

    /// Register an operation so callback settlement can reject and drain it.
    pub(crate) fn track<T, F>(&self, operation: F) -> TxFuture<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let guard = self.0.pending.enter();
        spawn_shared(async move {
            let result = operation.await;
            drop(guard);
            result
        })
    }

    fn read<T, F>(&self, method: &str, read: impl FnOnce(Arc<TxInner>) -> F) -> TxFuture<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        {
            let state = lock(&self.0.state);
            if state.sealed {
                return ready_shared(Err(settled()));
            }
            if state.has_table_write {
                return ready_shared(Err(DurableError::ReadAfterWrite(method.to_owned())));
            }
        }
        self.track(read(self.0.clone()))
    }

    pub(crate) fn write<T, F>(&self, write: impl FnOnce(Arc<TxInner>) -> F) -> TxFuture<T>
    where
        T: Clone + Send + Sync + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        {
            let mut state = lock(&self.0.state);
            if state.sealed {
                return ready_shared(Err(settled()));
            }
            state.has_table_write = true;
        }
        self.track(write(self.0.clone()))
    }

    /// Mark a synchronous table write and return the state.
    pub(crate) fn write_now(&self) -> DurableResult<MutexGuard<'_, TxState>> {
        let mut state = self.0.open_state()?;
        state.has_table_write = true;
        Ok(state)
    }

    pub fn conversation(&self, id: ConversationId) -> TxFuture<Option<ConversationRecord>> {
        self.read("conversation", move |tx| async move { tx.core.storage.conversation(id, &tx.context).await })
    }

    pub fn entry(&self, id: EntryId) -> TxFuture<Option<EntryRecord>> {
        self.read("entry", move |tx| async move {
            Ok(tx.core.storage.entry(id, &tx.context).await?.map(|stored| stored.entry))
        })
    }

    /// `None` when the entry is absent or has another kind.
    pub fn typed_entry<D>(&self, token: &Entry<D>, id: EntryId) -> TxFuture<Option<TypedEntry<D>>>
    where
        D: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
    {
        let token = token.clone();
        self.read("entry", move |tx| async move {
            match tx.core.storage.entry(id, &tx.context).await? {
                Some(stored) => token.decode(stored.entry),
                None => Ok(None),
            }
        })
    }

    pub fn task(&self, id: TaskId) -> TxFuture<Option<TaskRecord>> {
        self.read("task", move |tx| tx.committed_task(id))
    }

    pub fn scan_conversations(
        &self,
        query: ConversationQuery,
        limit: usize,
        cursor: Option<Cursor>,
    ) -> TxFuture<Page<ConversationRecord>> {
        self.read("scanConversations", move |tx| async move {
            tx.core.storage.scan_conversations(&query, limit, cursor.as_ref(), &tx.context).await
        })
    }

    pub fn scan_entries(&self, query: EntryQuery, limit: usize, cursor: Option<Cursor>) -> TxFuture<Page<EntryRecord>> {
        self.read("scanEntries", move |tx| async move {
            tx.core.storage.scan_entries(&query, limit, cursor.as_ref(), &tx.context).await
        })
    }

    /// Newest visible entry of the conversation that carries a `head`.
    pub fn latest_head_marker(&self, conversation_id: ConversationId) -> TxFuture<Option<EntryRecord>> {
        self.read("latestHeadMarker", move |tx| async move {
            tx.core.storage.find_latest_head_marker(conversation_id, None, &tx.context).await
        })
    }

    pub fn scan_tasks(&self, query: TaskQuery, limit: usize, cursor: Option<Cursor>) -> TxFuture<Page<TaskRecord>> {
        self.read("scanTasks", move |tx| async move {
            tx.core.storage.scan_tasks(&query, limit, cursor.as_ref(), &tx.context).await
        })
    }

    /// Internal: committed submission record.
    pub fn submission(&self, id: SubmissionId) -> TxFuture<Option<SubmissionRecord>> {
        self.read("submission", move |tx| async move { tx.core.storage.submission(id, &tx.context).await })
    }

    /// Committed submission with a conversation-scoped request ID.
    pub fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: impl Into<String>,
    ) -> TxFuture<Option<SubmissionRecord>> {
        let request_id = request_id.into();
        self.read("submissionByRequest", move |tx| async move {
            tx.core.storage.submission_by_request(conversation_id, &request_id, &tx.context).await
        })
    }
}
