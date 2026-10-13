//! Document addresses, scope scans, lifecycle atomicity, and lossless string identities.

use std::sync::Arc;

use super::documents::{change_delta, create, read, value};
use super::records::submission;
use super::{context, create_root, entry, expect_err, ids, json, mint, ops, pending_task};
use crate::ids::{DocumentId, EntryId, SubmissionId, TaskId};
use crate::types::{
    DocumentAddress, DocumentCreate, DocumentFork, DocumentHistory, DocumentPoint, DocumentQuery, DocumentScope,
    Storage, StorageWrite, SubmissionRecord, SubmissionStatus, TaskQuery, TaskRecord, TaskState,
};

fn document(id: DocumentId, kind: &str, key: Option<&str>, scope: DocumentScope) -> DocumentCreate {
    DocumentCreate { id, kind: kind.into(), key: key.map(Into::into), scope, history: None, fork: None }
}

fn address(kind: &str, key: Option<&str>, scope: DocumentScope) -> DocumentAddress {
    DocumentAddress { kind: kind.into(), scope, key: key.map(Into::into) }
}

async fn find(storage: &dyn Storage, address: &DocumentAddress, at: DocumentPoint) -> Option<DocumentId> {
    storage.find_document(address, at, context()).await.unwrap().map(|record| record.id)
}

pub async fn indexes_addresses_and_scopes(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let first_id: DocumentId = mint(storage).await;
    let second_id: DocumentId = mint(storage).await;
    let conversation_doc_id: DocumentId = mint(storage).await;
    let task_id: TaskId = mint(storage).await;
    let task_singleton: DocumentId = mint(storage).await;
    let task_family: DocumentId = mint(storage).await;
    let task_other_kind: DocumentId = mint(storage).await;
    let session = DocumentScope::Session;
    let conversation = DocumentScope::Conversation { conversation_id: root_id };
    let task = DocumentScope::Task { task_id };
    let conversation_doc = DocumentCreate {
        history: Some(DocumentHistory::Latest),
        fork: Some(DocumentFork::Current),
        ..document(conversation_doc_id, "cache", Some("__proto__"), conversation)
    };
    let writes = [
        StorageWrite::Task { value: pending_task(task_id, root_id, "ready") },
        create(&document(first_id, "cache", Some("__proto__"), session), 1, json(r#"{"owner":"first"}"#)),
        create(&document(second_id, "cache", Some("constructor"), session), 1, json(r#"{"owner":"second"}"#)),
        create(&conversation_doc, 1, json(r#"{"owner":"conversation"}"#)),
        create(&document(task_singleton, "task.cache", None, task), 1, json(r#"{"owner":"singleton"}"#)),
        create(&document(task_family, "task.cache", Some("member"), task), 1, json(r#"{"owner":"family"}"#)),
        create(&document(task_other_kind, "task.other", None, task), 1, json(r#"{"owner":"other"}"#)),
    ];
    let created_at = storage.commit(&writes, ctx).await.unwrap();

    let current = DocumentPoint::Current;
    assert_eq!(find(storage, &address("cache", Some("__proto__"), session), current).await, Some(first_id));
    let session_query = DocumentQuery { scope: session, at: current, kind: None };
    let first = storage.scan_documents(&session_query, 1, None, ctx).await.unwrap();
    assert_eq!(first.items.len(), 1);
    let second = storage.scan_documents(&session_query, 1, first.next.as_ref(), ctx).await.unwrap();
    let both: Vec<_> = first.items.iter().chain(&second.items).map(|record| record.id).collect();
    assert_eq!(both, [first_id, second_id]);
    let conversation_query = DocumentQuery { scope: conversation, at: current, kind: None };
    let page = storage.scan_documents(&conversation_query, 10, None, ctx).await.unwrap();
    assert_eq!(ids(&page.items, |record| record.id.0), [conversation_doc_id.0]);
    assert_eq!(find(storage, &address("task.cache", None, task), current).await, Some(task_singleton));
    assert_eq!(find(storage, &address("task.cache", Some("member"), task), current).await, Some(task_family));
    let task_query = DocumentQuery { scope: task, at: current, kind: Some("task.cache".into()) };
    let page = storage.scan_documents(&task_query, 10, None, ctx).await.unwrap();
    assert_eq!(ids(&page.items, |record| record.id.0), [task_singleton.0, task_family.0]);
    let historical = storage.document(task_singleton, DocumentPoint::At(created_at), ctx).await;
    expect_err(historical, "does not retain historical content");
}

pub async fn keeps_document_lifecycle_atomic(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let first_id: DocumentId = mint(storage).await;
    let second_id: DocumentId = mint(storage).await;
    let record = document(first_id, "singleton", None, DocumentScope::Session);
    storage.commit(&[create(&record, 1, json(r#"{"value":1}"#))], ctx).await.unwrap();
    let writes = [
        create(&DocumentCreate { id: second_id, ..record.clone() }, 1, json(r#"{"value":2}"#)),
        change_delta(first_id, 1, Vec::new()),
    ];
    expect_err(storage.commit(&writes, ctx).await, "already has a current incarnation");
    assert_eq!(value(storage, first_id, DocumentPoint::Current).await, Some(json(r#"{"value":1}"#)));
    assert_eq!(read(storage, second_id, DocumentPoint::Current).await, None);

    let empty_id: DocumentId = mint(storage).await;
    let conversation = DocumentScope::Conversation { conversation_id: root_id };
    let empty = DocumentCreate {
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::Initial),
        ..document(empty_id, &record.kind, Some("empty"), conversation)
    };
    let writes = [create(&empty, 1, json("{}")), StorageWrite::DocumentRetire { id: empty_id }];
    let empty_at = storage.commit(&writes, ctx).await.unwrap();
    assert_eq!(read(storage, empty_id, DocumentPoint::Current).await, None);
    assert_eq!(read(storage, empty_id, DocumentPoint::At(empty_at)).await, None);
    let empty_address = address(&record.kind, Some("empty"), conversation);
    assert_eq!(find(storage, &empty_address, DocumentPoint::At(empty_at)).await, None);
}

pub async fn rolls_back_tables_on_document_failure(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let task_id: TaskId = mint(storage).await;
    let submission_id: SubmissionId = mint(storage).await;
    let document_id: DocumentId = mint(storage).await;
    let task = pending_task(task_id, root_id, "ready");
    let queued = SubmissionRecord {
        request_id: Some("atomic".into()),
        ..submission(submission_id, root_id, SubmissionStatus::Queued)
    };
    let record = document(document_id, "atomic", None, DocumentScope::Session);
    let writes = [
        StorageWrite::Task { value: task.clone() },
        StorageWrite::Submission { value: queued.clone() },
        create(&record, 1, json(r#"{"count":1}"#)),
    ];
    let baseline_seq = storage.commit(&writes, ctx).await.unwrap();

    let entry_id: EntryId = mint(storage).await;
    let conflicting_id: DocumentId = mint(storage).await;
    let running =
        TaskRecord { state: TaskState::Running { checkpoint: json(r#"{"phase":"effect"}"#) }, ..task.clone() };
    let unanswered =
        SubmissionRecord { status: SubmissionStatus::Unanswered, reason: Some("failed".into()), ..queued.clone() };
    let writes = [
        StorageWrite::Task { value: running },
        StorageWrite::Submission { value: unanswered },
        StorageWrite::Entry { value: entry(entry_id, root_id, "transient") },
        create(&DocumentCreate { id: conflicting_id, ..record.clone() }, 1, json(r#"{"count":2}"#)),
    ];
    expect_err(storage.commit(&writes, ctx).await, "already has a current incarnation");

    assert_eq!(storage.task(task_id, ctx).await.unwrap(), Some(task.clone()));
    let pending = TaskQuery { status: Some(crate::types::TaskStatus::Pending), ..Default::default() };
    assert_eq!(storage.scan_tasks(&pending, 10, None, ctx).await.unwrap().items, [task]);
    assert_eq!(storage.submission_by_request(root_id, "atomic", ctx).await.unwrap(), Some(queued));
    assert_eq!(storage.entry(entry_id, ctx).await.unwrap(), None);
    assert_eq!(read(storage, conflicting_id, DocumentPoint::Current).await, None);
    let atomic_address = address("atomic", None, DocumentScope::Session);
    assert_eq!(find(storage, &atomic_address, DocumentPoint::Current).await, Some(document_id));
    let after = storage.commit(&[change_delta(document_id, 1, ops(r#"[["s",["count"],3]]"#))], ctx).await.unwrap();
    assert!(after > baseline_seq);
}

/// Upstream uses lone UTF-16 surrogates, which a lossy encoding would merge. Rust strings cannot hold them, so the
/// case uses identities that case folding, Unicode normalization, or NUL truncation would merge instead.
pub async fn keeps_string_identities_lossless(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let pairs = [("Kind", "kind"), ("\u{e9}", "e\u{301}"), ("a\0b", "a\0c")];
    for (first, second) in pairs {
        let first_task: TaskId = mint(storage).await;
        let second_task: TaskId = mint(storage).await;
        let first_submission: SubmissionId = mint(storage).await;
        let second_submission: SubmissionId = mint(storage).await;
        let document_ids: Vec<DocumentId> =
            vec![mint(storage).await, mint(storage).await, mint(storage).await, mint(storage).await];
        let request = |id, request: &str| SubmissionRecord {
            request_id: Some(request.into()),
            ..submission(id, root_id, SubmissionStatus::Queued)
        };
        let session = DocumentScope::Session;
        let writes = [
            StorageWrite::Task {
                value: TaskRecord { kind: first.into(), ..pending_task(first_task, root_id, "ready") },
            },
            StorageWrite::Task {
                value: TaskRecord { kind: second.into(), ..pending_task(second_task, root_id, "ready") },
            },
            StorageWrite::Submission { value: request(first_submission, first) },
            StorageWrite::Submission { value: request(second_submission, second) },
            create(&document(document_ids[0], first, None, session), 1, json(r#"{"identity":"first kind"}"#)),
            create(&document(document_ids[1], second, None, session), 1, json(r#"{"identity":"second kind"}"#)),
            create(&document(document_ids[2], "family", Some(first), session), 1, json(r#"{"identity":"first key"}"#)),
            create(
                &document(document_ids[3], "family", Some(second), session),
                1,
                json(r#"{"identity":"second key"}"#),
            ),
        ];
        storage.commit(&writes, ctx).await.unwrap();

        let by_kind = |kind: &str| TaskQuery { kind: Some(kind.into()), ..Default::default() };
        let page = storage.scan_tasks(&by_kind(first), 10, None, ctx).await.unwrap();
        assert_eq!(ids(&page.items, |task| task.id.0), [first_task.0]);
        let page = storage.scan_tasks(&by_kind(second), 10, None, ctx).await.unwrap();
        assert_eq!(ids(&page.items, |task| task.id.0), [second_task.0]);
        assert_eq!(storage.task(first_task, ctx).await.unwrap().unwrap().kind, first);
        assert_eq!(storage.task(second_task, ctx).await.unwrap().unwrap().kind, second);
        let found = storage.submission_by_request(root_id, first, ctx).await.unwrap().unwrap();
        assert_eq!((found.request_id.as_deref(), found.id), (Some(first), first_submission));
        let found = storage.submission_by_request(root_id, second, ctx).await.unwrap().unwrap();
        assert_eq!(found.id, second_submission);
        let current = DocumentPoint::Current;
        assert_eq!(find(storage, &address(first, None, session), current).await, Some(document_ids[0]));
        assert_eq!(find(storage, &address(second, None, session), current).await, Some(document_ids[1]));
        assert_eq!(find(storage, &address("family", Some(first), session), current).await, Some(document_ids[2]));
        assert_eq!(find(storage, &address("family", Some(second), session), current).await, Some(document_ids[3]));
        let query = DocumentQuery { scope: session, at: current, kind: Some(first.into()) };
        let page = storage.scan_documents(&query, 10, None, ctx).await.unwrap();
        assert_eq!(ids(&page.items, |record| record.id.0), [document_ids[0].0]);
    }
}
