//! The Session's Storage behind its failure guard (spec section 10.1). Every component reads and writes through it.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use pi_chord::Context;

use super::line::PendingCounter;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::types::*;

/// What the guard needs of its Session: the failure so far, and how to fail it.
pub(crate) trait FailureLatch: Send + Sync {
    fn failure(&self) -> Option<DurableError>;
    fn fail(&self, error: DurableError);
}

/// A failed Session gets `SessionFailed` without reaching the backend. A call that throws fails the Session and
/// returns the backend's error, unless it is a `StorageRequest` error or a read whose caller's context was aborted.
/// `close()` always reaches the backend, so a failed Session still closes it.
pub(crate) struct GuardedStorage {
    storage: Arc<dyn Storage>,
    latch: Weak<dyn FailureLatch>,
    /// Backend calls underway, also those off the Session line; close waits for them.
    underway: Arc<PendingCounter>,
    closing: AtomicBool,
}

impl GuardedStorage {
    pub fn new(storage: Arc<dyn Storage>, latch: Weak<dyn FailureLatch>) -> Self {
        Self { storage, latch, underway: Arc::default(), closing: AtomicBool::new(false) }
    }

    fn assert_healthy(&self) -> DurableResult<()> {
        match self.latch.upgrade().and_then(|latch| latch.failure()) {
            Some(cause) => Err(DurableError::session_failed(cause)),
            None => Ok(()),
        }
    }

    /// Run one backend call. `read` is a read's context; `commit()` and `mint_id()` pass none, so nothing exempts
    /// their errors. A call still underway when another fails the Session ends with `SessionFailed` too.
    async fn call<T>(&self, read: Option<&Context>, run: impl Future<Output = DurableResult<T>>) -> DurableResult<T> {
        self.assert_healthy()?;
        if self.closing.load(Ordering::SeqCst) {
            return Err(DurableError::error("Session is closed"));
        }
        let guard = self.underway.enter();
        let result = run.await;
        drop(guard);
        match result {
            Ok(value) => {
                self.assert_healthy()?;
                Ok(value)
            }
            Err(error) => {
                self.assert_healthy()?;
                let exempt = read.is_some_and(|context| error.is_storage_request() || context.is_cancelled());
                if !exempt && let Some(latch) = self.latch.upgrade() {
                    latch.fail(error.clone());
                }
                Err(error)
            }
        }
    }
}

#[async_trait]
impl Storage for GuardedStorage {
    async fn commit(&self, writes: &[StorageWrite], context: &Context) -> DurableResult<Seq> {
        self.call(None, self.storage.commit(writes, context)).await
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.call(None, self.storage.mint_id()).await
    }

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>> {
        self.call(Some(context), self.storage.conversation(id, context)).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        self.call(Some(context), self.storage.scan_conversations(query, limit, cursor, context)).await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>> {
        self.call(Some(context), self.storage.entry(id, context)).await
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        self.call(Some(context), self.storage.visible_entry(conversation_id, id, context)).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        self.call(Some(context), self.storage.find_latest_head_marker(conversation_id, at_or_before, context)).await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        self.call(Some(context), self.storage.scan_entries(query, limit, cursor, context)).await
    }

    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.call(Some(context), self.storage.task(id, context)).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        self.call(Some(context), self.storage.scan_tasks(query, limit, cursor, context)).await
    }

    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        self.call(Some(context), self.storage.submission(id, context)).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        self.call(Some(context), self.storage.scan_submissions(query, limit, cursor, context)).await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        self.call(Some(context), self.storage.submission_by_request(conversation_id, request_id, context)).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        self.call(Some(context), self.storage.find_document(address, at, context)).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.call(Some(context), self.storage.document(id, at, context)).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        self.call(Some(context), self.storage.scan_documents(query, limit, cursor, context)).await
    }

    /// Close the backend once every call underway has settled; new calls are refused from here on.
    async fn close(&self, context: &Context) -> DurableResult<()> {
        self.closing.store(true, Ordering::SeqCst);
        self.underway.drained().await;
        self.storage.close(context).await
    }
}
