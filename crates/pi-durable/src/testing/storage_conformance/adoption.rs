//! The adoption contract of `Storage::commit()`: a check every case runs under, and its own case.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pi_chord::Context;

use super::{context, create_root, entry, expect_err, mint};
use crate::errors::DurableResult;
use crate::ids::{ConversationId, DocumentId, EntryId, Seq, SubmissionId, TaskId};
use crate::types::*;

/// `storage` with every commit checked against the adoption contract: `adopt` runs exactly once, with the committed
/// sequence, before the commit returns, and never for a commit that fails. The cases fail commits only before they
/// are durable.
pub fn check_adoption(storage: Arc<dyn Storage>) -> Arc<dyn Storage> {
    Arc::new(CheckedAdoption(storage))
}

struct CheckedAdoption(Arc<dyn Storage>);

#[async_trait]
impl Storage for CheckedAdoption {
    async fn commit(&self, writes: &[StorageWrite], adopt: Adopt<'_>, context: &Context) -> DurableResult<Seq> {
        let adopted = Mutex::new((Vec::new(), false));
        let recording = |seq: Seq| {
            let mut adopted = adopted.lock().unwrap();
            assert!(!adopted.1, "Storage.commit() adopted after it settled");
            adopted.0.push(seq);
            adopt(seq);
        };
        let result = self.0.commit(writes, &recording, context).await;
        let mut adopted = adopted.lock().unwrap();
        adopted.1 = true;
        match &result {
            Ok(seq) => assert_eq!(adopted.0, [*seq]),
            Err(_) => assert_eq!(adopted.0, []),
        }
        result
    }

    async fn mint_id(&self) -> DurableResult<u64> {
        self.0.mint_id().await
    }

    async fn conversation(&self, id: ConversationId, context: &Context) -> DurableResult<Option<ConversationRecord>> {
        self.0.conversation(id, context).await
    }

    async fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<ConversationRecord>> {
        self.0.scan_conversations(query, limit, cursor, context).await
    }

    async fn entry(&self, id: EntryId, context: &Context) -> DurableResult<Option<StoredEntry>> {
        self.0.entry(id, context).await
    }

    async fn visible_entry(
        &self,
        conversation_id: ConversationId,
        id: EntryId,
        context: &Context,
    ) -> DurableResult<Option<StoredEntry>> {
        self.0.visible_entry(conversation_id, id, context).await
    }

    async fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
        context: &Context,
    ) -> DurableResult<Option<EntryRecord>> {
        self.0.find_latest_head_marker(conversation_id, at_or_before, context).await
    }

    async fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<EntryRecord>> {
        self.0.scan_entries(query, limit, cursor, context).await
    }

    async fn task(&self, id: TaskId, context: &Context) -> DurableResult<Option<TaskRecord>> {
        self.0.task(id, context).await
    }

    async fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<TaskRecord>> {
        self.0.scan_tasks(query, limit, cursor, context).await
    }

    async fn submission(&self, id: SubmissionId, context: &Context) -> DurableResult<Option<SubmissionRecord>> {
        self.0.submission(id, context).await
    }

    async fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<SubmissionRecord>> {
        self.0.scan_submissions(query, limit, cursor, context).await
    }

    async fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
        context: &Context,
    ) -> DurableResult<Option<SubmissionRecord>> {
        self.0.submission_by_request(conversation_id, request_id, context).await
    }

    async fn find_document(
        &self,
        address: &DocumentAddress,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<DocumentRecord>> {
        self.0.find_document(address, at, context).await
    }

    async fn document(
        &self,
        id: DocumentId,
        at: DocumentPoint,
        context: &Context,
    ) -> DurableResult<Option<StoredDocument>> {
        self.0.document(id, at, context).await
    }

    async fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
        context: &Context,
    ) -> DurableResult<Page<DocumentRecord>> {
        self.0.scan_documents(query, limit, cursor, context).await
    }

    async fn close(&self, context: &Context) -> DurableResult<()> {
        self.0.close(context).await
    }
}

/// Upstream "adopts a durable commit once before resolving, and never a failed one".
pub async fn adopts_durable_commits_once(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let events = Mutex::new(Vec::<String>::new());
    let record = |seq: Seq| events.lock().unwrap().push(format!("adopt {}", seq.0));
    let entry_id: EntryId = mint(storage).await;
    let writes = [StorageWrite::Entry { value: entry(entry_id, root_id, "note") }];
    let seq = storage.commit(&writes, &record, context()).await.unwrap();
    events.lock().unwrap().push(format!("resolve {}", seq.0));
    assert_eq!(*events.lock().unwrap(), [format!("adopt {}", seq.0), format!("resolve {}", seq.0)]);
    // The batch is durable when it is adopted.
    assert_eq!(storage.entry(entry_id, context()).await.unwrap().map(|stored| stored.commit_seq), Some(seq));

    events.lock().unwrap().clear();
    let root = ConversationRecord { id: root_id, parent: None, owner: None };
    let failed = storage.commit(&[StorageWrite::Conversation { value: root }], &record, context()).await;
    expect_err(failed, &format!("ID {root_id} already belongs to conversation"));
    assert!(events.lock().unwrap().is_empty());

    let next_id: EntryId = mint(storage).await;
    let writes = [StorageWrite::Entry { value: entry(next_id, root_id, "note") }];
    let next = storage.commit(&writes, &record, context()).await.unwrap();
    assert_eq!(*events.lock().unwrap(), [format!("adopt {}", next.0)]);
    assert!(next > seq);
}
