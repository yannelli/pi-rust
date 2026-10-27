//! A `Storage` that closes and reopens its backend after every commit, so conformance also covers reopen.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use pi_chord::{BACKGROUND_CONTEXT, Context};
use pi_durable::*;
use tokio::sync::Mutex;

/// Opens the backend again from its persisted state.
pub type Reopen = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<dyn Storage>> + Send>> + Send + Sync>;

pub struct ReopeningStorage {
    current: Mutex<Arc<dyn Storage>>,
    reopen: Reopen,
    closed: std::sync::atomic::AtomicBool,
}

impl ReopeningStorage {
    pub async fn new(reopen: Reopen) -> Self {
        let current = reopen().await;
        Self { current: Mutex::new(current), reopen, closed: Default::default() }
    }

    async fn current(&self) -> Arc<dyn Storage> {
        self.current.lock().await.clone()
    }
}

#[async_trait]
impl Storage for ReopeningStorage {
    async fn commit(&self, writes: &[StorageWrite], adopt: Adopt<'_>, context: &Context) -> DurableResult<Seq> {
        let mut current = self.current.lock().await;
        let result = current.commit(writes, adopt, context).await;
        if !self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            current.close(&BACKGROUND_CONTEXT).await.unwrap();
            *current = (self.reopen)().await;
        }
        result
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.current().await.mint_id().await
    }

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>> {
        self.current().await.conversation(id, context).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        self.current().await.scan_conversations(query, limit, cursor, context).await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>> {
        self.current().await.entry(id, context).await
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        self.current().await.visible_entry(conversation_id, id, context).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        self.current().await.find_latest_head_marker(conversation_id, at_or_before, context).await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        self.current().await.scan_entries(query, limit, cursor, context).await
    }

    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.current().await.task(id, context).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        self.current().await.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        self.current().await.submission(id, context).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        self.current().await.scan_submissions(query, limit, cursor, context).await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        self.current().await.submission_by_request(conversation_id, request_id, context).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        self.current().await.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.current().await.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        self.current().await.scan_documents(query, limit, cursor, context).await
    }

    async fn close(&self, context: &Context) -> DurableResult<()> {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        self.current().await.close(context).await
    }
}
