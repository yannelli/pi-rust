//! Port of upstream `test/session-support.ts`: memory storage with observable commits, held calls, and injected
//! commit failures, plus a Session opened over it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use pi_chord::{BACKGROUND_CONTEXT, Context, Json};
use pi_durable::session::{Session, SessionOptions, create_session};
use pi_durable::storage::MemoryStorage;
use pi_durable::*;
use tokio::sync::{Notify, watch};

pub fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

/// A gate that holds calls until released and reports when the first held call arrives.
#[derive(Clone)]
pub struct Gate {
    entered: watch::Sender<bool>,
    released: Arc<(AtomicBool, Notify)>,
}

impl Gate {
    fn new() -> Self {
        Self { entered: watch::channel(false).0, released: Arc::new((AtomicBool::new(false), Notify::new())) }
    }

    pub async fn entered(&self) {
        let mut receiver = self.entered.subscribe();
        let _ = receiver.wait_for(|entered| *entered).await;
    }

    pub fn release(&self) {
        self.released.0.store(true, Ordering::SeqCst);
        self.released.1.notify_waiters();
    }

    async fn pass(&self) {
        self.entered.send_replace(true);
        loop {
            let notified = self.released.1.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.released.0.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

type CommitFilter = Arc<dyn Fn(&[StorageWrite]) -> bool + Send + Sync>;

/// One Storage call a test hook sees: `task`, `conversation`, `scan_entries`, `scan_queued_submissions`, `mint_id`, `submission`, or `close`.
pub struct StorageCall {
    pub method: &'static str,
    /// The id a `conversation` call reads.
    pub id: Option<u64>,
    pub limit: usize,
    pub first_page: bool,
    pub context: Context,
}

pub type CallHook = Arc<dyn Fn(StorageCall) -> futures::future::BoxFuture<'static, DurableResult<()>> + Send + Sync>;

/// Memory storage with observable commits, held calls, and injected commit failures.
#[derive(Default)]
pub struct ControlledStorage {
    inner: MemoryStorage,
    /// Batches admitted by the Session, in order.
    pub commits: Mutex<Vec<Vec<StorageWrite>>>,
    pub mint_count: AtomicUsize,
    pub document_read_count: AtomicUsize,
    /// Entry rows returned by `scan_entries`.
    pub scanned_rows: AtomicUsize,
    commit_gate: Mutex<Option<Gate>>,
    /// When set, the commit gate holds only the batches it accepts.
    commit_filter: Mutex<Option<CommitFilter>>,
    find_gate: Mutex<Option<Gate>>,
    commit_failure: Mutex<Option<DurableError>>,
    /// When set, the first batch the filter accepts fails with the error, once.
    filtered_failure: Mutex<Option<(CommitFilter, DurableError)>>,
    /// When set, `close()` closes and then fails with this error.
    pub close_failure: Mutex<Option<DurableError>>,
    /// Runs before `task`, `scan_entries`, `mint_id`, `submission`, and `close`; an error fails that call.
    pub call_hook: Mutex<Option<CallHook>>,
    closed: AtomicBool,
}

impl ControlledStorage {
    pub fn commits(&self) -> Vec<Vec<StorageWrite>> {
        self.commits.lock().unwrap().clone()
    }

    pub fn commit_count(&self) -> usize {
        self.commits.lock().unwrap().len()
    }

    pub fn last_commit(&self) -> Vec<StorageWrite> {
        self.commits.lock().unwrap().last().cloned().expect("a commit")
    }

    pub fn mints(&self) -> usize {
        self.mint_count.load(Ordering::SeqCst)
    }

    pub fn document_reads(&self) -> usize {
        self.document_read_count.load(Ordering::SeqCst)
    }

    pub fn hold_commits(&self) -> Gate {
        let gate = Gate::new();
        *self.commit_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// Drop the commit gate as a crash would: a held commit never lands, and later commits pass.
    pub fn crash(&self) {
        *self.commit_gate.lock().unwrap() = None;
        *self.commit_filter.lock().unwrap() = None;
    }

    /// Hold only the commits whose batch `filter` accepts.
    pub fn hold_commits_where(&self, filter: impl Fn(&[StorageWrite]) -> bool + Send + Sync + 'static) -> Gate {
        *self.commit_filter.lock().unwrap() = Some(Arc::new(filter));
        self.hold_commits()
    }

    pub fn hold_find_document(&self) -> Gate {
        let gate = Gate::new();
        *self.find_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    pub fn fail_commit_where(
        &self,
        filter: impl Fn(&[StorageWrite]) -> bool + Send + Sync + 'static,
        error: DurableError,
    ) {
        *self.filtered_failure.lock().unwrap() = Some((Arc::new(filter), error));
    }

    pub fn fail_next_commit(&self, error: DurableError) {
        *self.commit_failure.lock().unwrap() = Some(error);
    }

    /// Reopen after `close()`, as a fresh process would reopen the same database.
    pub fn reopen(&self) -> &Self {
        self.closed.store(false, Ordering::SeqCst);
        self
    }

    async fn hook(&self, method: &'static str, limit: usize, first_page: bool, context: &Context) -> DurableResult<()> {
        self.hook_id(method, limit, first_page, None, context).await
    }

    async fn hook_id(
        &self,
        method: &'static str,
        limit: usize,
        first_page: bool,
        id: Option<u64>,
        context: &Context,
    ) -> DurableResult<()> {
        let hook = self.call_hook.lock().unwrap().clone();
        match hook {
            Some(hook) => hook(StorageCall { method, id, limit, first_page, context: context.clone() }).await,
            None => Ok(()),
        }
    }

    fn open(&self) -> DurableResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(DurableError::storage("MemoryStorage is closed"));
        }
        Ok(())
    }

    fn take_gate(slot: &Mutex<Option<Gate>>) -> Option<Gate> {
        let mut slot = slot.lock().unwrap();
        let gate = slot.clone();
        if gate.as_ref().is_some_and(|gate| gate.released.0.load(Ordering::SeqCst)) {
            *slot = None;
            return None;
        }
        gate
    }
}

#[async_trait]
impl Storage for ControlledStorage {
    async fn commit(&self, writes: &[StorageWrite], context: &Context) -> DurableResult<Seq> {
        let failure = self.filtered_failure.lock().unwrap().take();
        match failure {
            Some((filter, error)) if filter(writes) => return Err(error),
            failure => *self.filtered_failure.lock().unwrap() = failure,
        }
        self.commits.lock().unwrap().push(writes.to_vec());
        let filter = self.commit_filter.lock().unwrap().clone();
        if filter.is_none_or(|filter| filter(writes))
            && let Some(gate) = Self::take_gate(&self.commit_gate)
        {
            gate.pass().await;
        }
        if let Some(error) = self.commit_failure.lock().unwrap().take() {
            return Err(error);
        }
        self.open()?;
        self.inner.commit(writes, context).await
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.mint_count.fetch_add(1, Ordering::SeqCst);
        self.hook("mint_id", 0, false, &context()).await?;
        self.open()?;
        self.inner.mint_id().await
    }

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>> {
        self.hook_id("conversation", 0, false, Some(id.0), context).await?;
        self.open()?;
        self.inner.conversation(id, context).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        self.open()?;
        self.inner.scan_conversations(query, limit, cursor, context).await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>> {
        self.open()?;
        self.inner.entry(id, context).await
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        self.open()?;
        self.inner.visible_entry(conversation_id, id, context).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        self.open()?;
        self.inner.find_latest_head_marker(conversation_id, at_or_before, context).await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        self.hook("scan_entries", limit, cursor.is_none(), context).await?;
        self.open()?;
        let page = self.inner.scan_entries(query, limit, cursor, context).await?;
        self.scanned_rows.fetch_add(page.items.len(), Ordering::SeqCst);
        Ok(page)
    }

    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.hook("task", 0, false, context).await?;
        self.open()?;
        self.inner.task(id, context).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        self.open()?;
        self.inner.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        self.hook("submission", 0, false, context).await?;
        self.open()?;
        self.inner.submission(id, context).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        if query.status == Some(SubmissionStatus::Queued) {
            self.hook("scan_queued_submissions", limit, cursor.is_none(), context).await?;
        }
        self.open()?;
        self.inner.scan_submissions(query, limit, cursor, context).await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        self.open()?;
        self.inner.submission_by_request(conversation_id, request_id, context).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        if let Some(gate) = Self::take_gate(&self.find_gate) {
            gate.pass().await;
        }
        self.open()?;
        self.inner.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.document_read_count.fetch_add(1, Ordering::SeqCst);
        self.open()?;
        self.inner.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        self.open()?;
        self.inner.scan_documents(query, limit, cursor, context).await
    }

    async fn close(&self, context: &Context) -> DurableResult<()> {
        self.hook("close", 0, false, context).await?;
        self.closed.store(true, Ordering::SeqCst);
        match self.close_failure.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// A Session over controlled storage and every committed publication.
pub struct TestSession {
    pub storage: Arc<ControlledStorage>,
    pub session: Session,
    pub publications: Arc<Mutex<Vec<CommitPublication>>>,
}

impl TestSession {
    pub fn last_publication(&self) -> CommitPublication {
        self.publications.lock().unwrap().last().cloned().expect("a publication")
    }

    pub fn publication_count(&self) -> usize {
        self.publications.lock().unwrap().len()
    }
}

pub fn open_test_session() -> TestSession {
    let storage = Arc::new(ControlledStorage::default());
    let session = create_session(storage.clone(), SessionOptions::default());
    let publications = Arc::new(Mutex::new(Vec::new()));
    let sink = publications.clone();
    session
        .subscribe_commits(move |publication, _| {
            sink.lock().unwrap().push(publication.clone());
            Ok(())
        })
        .unwrap();
    TestSession { storage, session, publications }
}

/// Document value changes of a publication: `(record, conversation, version, value, ops)`.
pub fn document_changes(publication: &CommitPublication) -> Vec<DocumentCommitChange> {
    let documents = publication.changes.iter().filter_map(|change| match change {
        CommitChange::Document(document @ DocumentCommitChange::Document { .. }) => Some(document.clone()),
        _ => None,
    });
    documents.collect()
}

pub fn document_copy_changes(publication: &CommitPublication) -> Vec<DocumentCommitChange> {
    let copies = publication.changes.iter().filter_map(|change| match change {
        CommitChange::Document(copy @ DocumentCommitChange::Copy { .. }) => Some(copy.clone()),
        _ => None,
    });
    copies.collect()
}

/// The value of a document change; `None` for retirement.
pub fn change_value(change: &DocumentCommitChange) -> Option<Json> {
    match change {
        DocumentCommitChange::Document { value, .. } => value.clone(),
        DocumentCommitChange::Copy { .. } => None,
    }
}

pub fn change_record(change: &DocumentCommitChange) -> &DocumentRecord {
    match change {
        DocumentCommitChange::Document { record, .. } | DocumentCommitChange::Copy { record, .. } => record,
    }
}

/// Create one conversation and return its ID.
pub async fn create_conversation(session: &Session) -> ConversationId {
    session
        .commit(|tx| async move { Ok(tx.create_conversation(ConversationOwnership::Ownerless).await?.id) }, &context())
        .await
        .unwrap()
}

/// Let spawned tasks run: pending deliveries, watch callbacks, and state frames.
pub async fn flush() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

/// Parse a JSON literal.
pub fn json(text: &str) -> Json {
    Json::parse(text).unwrap()
}

/// The error message of a result that must have failed.
pub fn message<T>(result: DurableResult<T>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.to_string(),
    }
}

pub fn document_writes(writes: &[StorageWrite]) -> Vec<StorageWrite> {
    let documents = writes.iter().filter(|write| {
        matches!(
            write,
            StorageWrite::DocumentCreate { .. }
                | StorageWrite::DocumentChange { .. }
                | StorageWrite::DocumentRetire { .. }
        )
    });
    documents.cloned().collect()
}

/// `"base"` or `"delta"` for a document change write.
pub fn content_kind(write: &StorageWrite) -> Option<&'static str> {
    match write {
        StorageWrite::DocumentChange { content, .. } | StorageWrite::DocumentCreate { content, .. } => {
            Some(match content {
                DocumentContent::Base { .. } => "base",
                DocumentContent::Delta { .. } => "delta",
            })
        }
        _ => None,
    }
}

/// A one-shot signal tests resolve and await (upstream `deferred()`).
#[derive(Clone)]
pub struct Deferred(watch::Sender<bool>);

impl Default for Deferred {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl Deferred {
    pub fn resolve(&self) {
        self.0.send_replace(true);
    }

    pub async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|done| *done).await;
    }
}
