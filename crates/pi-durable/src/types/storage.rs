//! The atomic persistence boundary for Session records (spec section 10).

use async_trait::async_trait;
use pi_chord::delta::Op;
use pi_chord::{Context, Json, JsonMap};
use serde::{Deserialize, Serialize};

use super::documents::{
    DocumentAddress, DocumentContent, DocumentCopySource, DocumentCreate, DocumentPoint, DocumentQuery, DocumentRecord,
    StoredDocument,
};
use super::records::{ConversationRecord, EntryRecord, SubmissionRecord, SubmissionStatus};
use super::tasks::{TaskRecord, TaskStatus};
use crate::errors::DurableResult;
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};

/// Backend-owned JSON continuation state that callers only round-trip to the same scan. It carries the scan's order.
pub type Cursor = JsonMap;

/// One ordered scan result and its optional continuation state.
#[derive(Clone, Debug, PartialEq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, next: Option<Cursor>) -> Self {
        Self { items, next }
    }

    pub fn empty() -> Self {
        Self { items: Vec::new(), next: None }
    }
}

/// ID order of a scan: `ascending` is oldest first, `descending` newest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanOrder {
    Ascending,
    Descending,
}

impl ScanOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ascending => "ascending",
            Self::Descending => "descending",
        }
    }
}

/// Optional filters for an ordered conversation scan. `order` defaults to ascending; with a cursor, the cursor's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConversationQuery {
    pub owner_conversation_id: Option<ConversationId>,
    pub owner_task_id: Option<TaskId>,
    pub order: Option<ScanOrder>,
}

/// Inclusive ID bounds for a scan of one conversation's fork-aware history. `order` defaults to descending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryQuery {
    pub conversation_id: ConversationId,
    /// Oldest entry ID that may be returned.
    pub min_entry_id: Option<EntryId>,
    /// Newest entry ID that may be returned.
    pub max_entry_id: Option<EntryId>,
    pub order: Option<ScanOrder>,
}

impl EntryQuery {
    pub fn new(conversation_id: ConversationId) -> Self {
        Self { conversation_id, min_entry_id: None, max_entry_id: None, order: None }
    }
}

/// Optional filters for an ordered scan of task records. `order` defaults to ascending.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskQuery {
    pub conversation_id: Option<ConversationId>,
    pub kind: Option<String>,
    pub status: Option<TaskStatus>,
    pub abort_requested: Option<bool>,
    pub background: Option<bool>,
    pub order: Option<ScanOrder>,
}

/// Optional filters for an ordered scan of submission records. `order` defaults to ascending.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SubmissionQuery {
    pub conversation_id: Option<ConversationId>,
    pub status: Option<SubmissionStatus>,
    pub order: Option<ScanOrder>,
}

/// One entry and the sequence of the commit that persisted it.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredEntry {
    pub entry: EntryRecord,
    pub commit_seq: Seq,
}

/// One record or document mutation in an atomic storage commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StorageWrite {
    #[serde(rename = "conversation")]
    Conversation { value: ConversationRecord },
    #[serde(rename = "entry")]
    Entry { value: EntryRecord },
    #[serde(rename = "task")]
    Task { value: TaskRecord },
    #[serde(rename = "submission")]
    Submission { value: SubmissionRecord },
    /// `content` is always a base.
    #[serde(rename = "document.create")]
    DocumentCreate { record: DocumentCreate, content: DocumentContent },
    #[serde(rename = "document.copy")]
    DocumentCopy { record: DocumentCreate, source: DocumentCopySource },
    #[serde(rename = "document.change")]
    DocumentChange { id: DocumentId, content: DocumentContent },
    #[serde(rename = "document.retire")]
    DocumentRetire { id: DocumentId },
}

/// Committed change of one document incarnation.
#[derive(Clone, Debug, PartialEq)]
pub enum DocumentCommitChange {
    Document {
        record: DocumentRecord,
        /// Conversation owning the document; task documents derive it from their task. `None` for Session documents.
        conversation_id: Option<ConversationId>,
        /// Definition version of `value`; `None` when this commit retired the incarnation.
        version: Option<u32>,
        /// Exact adopted revision, or `None` when this commit retired the incarnation.
        value: Option<Json>,
        /// Exact adopted operations for an ordinary update; empty for creation and retirement.
        ops: Vec<Op>,
    },
    /// Definition-free child initialization; consumers hydrate through state or watch acquisition.
    Copy { record: DocumentRecord, conversation_id: ConversationId, source: DocumentCopySource },
}

/// One change of a commit publication: a complete table record or a document change.
#[derive(Clone, Debug, PartialEq)]
pub enum CommitChange {
    Conversation(ConversationRecord),
    Entry(EntryRecord),
    Task(TaskRecord),
    Submission(SubmissionRecord),
    Document(DocumentCommitChange),
}

/// Every immutable change from one successful Session commit. Change order is unspecified.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitPublication {
    pub seq: Seq,
    pub changes: Vec<CommitChange>,
}

/// Atomic persistence boundary for Session records. An error fails the Session, except a `StorageRequest` error
/// or a cancellation from a read. Any error from `commit()` or `mint_id()` is fatal.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Atomically persist one batch and return its sequence. Once it returns, later reads see it.
    async fn commit(&self, writes: &[StorageWrite], context: &Context) -> DurableResult<Seq>;

    /// A fresh ID above every ID minted or stored before, also across reopen: scans order by ID as creation order.
    async fn mint_id(&self) -> DurableResult<u64>;

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>>;

    /// Scan conversations in `query.order` (default ascending) by ID.
    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>>;

    /// Look up one global entry and the sequence of the commit that persisted it.
    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>>;

    /// Look up one entry only when it is visible through the conversation's ancestry.
    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>>;

    /// The newest visible entry with a `head` at or below the optional inclusive cutoff. Its `head` is the range's
    /// actual lower bound.
    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>>;

    /// Scan the inclusive visible range in `query.order` (default descending), at most `limit` entries.
    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>>;

    /// The latest complete record of one task.
    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>>;

    /// Scan task records matching every supplied filter in `query.order` (default ascending) by ID.
    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>>;

    /// The latest complete record of one admitted submission.
    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>>;

    /// Scan submissions matching every supplied filter in `query.order` (default ascending) by ID.
    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>>;

    /// Find a submission by its conversation-scoped host deduplication key.
    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>>;

    /// The incarnation occupying one exact logical address at the selected point.
    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>>;

    /// Materialize one incarnation by ID at the selected point without following a replacement at its address.
    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>>;

    /// Scan incarnations alive in one exact scope at the selected point, by ascending ID.
    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>>;

    /// Release backend resources; all later operations must fail.
    async fn close(&self, context: &Context) -> DurableResult<()>;
}
