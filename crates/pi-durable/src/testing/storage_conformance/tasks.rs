//! Task and submission records: complete replacement and filtered scans.

use std::sync::Arc;

use pi_chord::JsonMap;

use super::records::submission;
use super::{context, create_root, ids, json, mint, pending_task};
use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::types::{
    ConversationRecord, Cursor, JoinPolicy, Storage, StorageWrite, SubmissionQuery, SubmissionRecord, SubmissionStatus,
    SubmissionType, TaskOutcome, TaskOutcomeError, TaskQuery, TaskRecord, TaskState, TaskStatus,
};

fn task_write(value: &TaskRecord) -> StorageWrite {
    StorageWrite::Task { value: value.clone() }
}

fn submission_write(value: &SubmissionRecord) -> StorageWrite {
    StorageWrite::Submission { value: value.clone() }
}

async fn task_ids(
    storage: &dyn Storage,
    query: &TaskQuery,
    limit: usize,
    cursor: Option<&Cursor>,
) -> (Vec<u64>, Option<Cursor>) {
    let page = storage.scan_tasks(query, limit, cursor, context()).await.unwrap();
    (ids(&page.items, |task| task.id.0), page.next)
}

pub async fn replaces_task_records(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let first_id: TaskId = mint(storage).await;
    let second_id: TaskId = mint(storage).await;
    let third_id: TaskId = mint(storage).await;
    let mut memos = JsonMap::new();
    memos.insert("winner", "first");
    let first = TaskRecord { memos: Some(memos), ..pending_task(first_id, root_id, "ready") };
    let second = TaskRecord { background: true, ..pending_task(second_id, root_id, "ready") };
    let third = TaskRecord { abort_requested: true, ..pending_task(third_id, root_id, "ready") };
    storage.commit(&[task_write(&first), task_write(&second), task_write(&third)], ctx).await.unwrap();

    let running = TaskRecord {
        state: TaskState::Running { checkpoint: json(r#"{"phase":"effect","attempt":1}"#) },
        abort_requested: true,
        ..first.clone()
    };
    storage.commit(&[task_write(&running)], ctx).await.unwrap();
    assert_eq!(storage.task(first_id, ctx).await.unwrap(), Some(running));
    let terminal = TaskRecord {
        state: TaskState::Terminal { outcome: TaskOutcome::completed(json(r#"{"entryId":99}"#)) },
        abort_requested: true,
        memos: None,
        ..first
    };
    storage.commit(&[task_write(&terminal)], ctx).await.unwrap();
    assert_eq!(storage.task(first_id, ctx).await.unwrap(), Some(terminal.clone()));

    let pending = TaskQuery { status: Some(TaskStatus::Pending), ..Default::default() };
    let (page, next) = task_ids(storage, &pending, 1, None).await;
    assert_eq!(page, [second_id.0]);
    assert!(next.is_some());
    assert_eq!(task_ids(storage, &pending, 1, next.as_ref()).await.0, [third_id.0]);
    let aborted_terminal =
        TaskQuery { status: Some(TaskStatus::Terminal), abort_requested: Some(true), ..Default::default() };
    assert_eq!(storage.scan_tasks(&aborted_terminal, 10, None, ctx).await.unwrap().items, [terminal]);
    let background = TaskQuery { background: Some(true), ..Default::default() };
    assert_eq!(task_ids(storage, &background, 10, None).await.0, [second_id.0]);
}

pub async fn scans_waiting_and_completing_tasks(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let owner_id: TaskId = mint(storage).await;
    let waiting_id: TaskId = mint(storage).await;
    let completing_id: TaskId = mint(storage).await;
    let owner = pending_task(owner_id, root_id, "ready");
    let mut memos = JsonMap::new();
    memos.insert("kept", true);
    let waiting = TaskRecord {
        owner: Some(owner_id),
        state: TaskState::Waiting {
            checkpoint: json(r#"{"phase":"next"}"#),
            on: vec![owner_id],
            policy: JoinPolicy::AllSettled,
        },
        memos: Some(memos),
        ..pending_task(waiting_id, root_id, "ready")
    };
    let outcome = TaskOutcome::Failed { error: TaskOutcomeError::new("held"), result: None };
    let completing = TaskRecord {
        owner: Some(owner_id),
        state: TaskState::Completing { outcome: outcome.clone() },
        ..pending_task(completing_id, root_id, "ready")
    };
    storage.commit(&[task_write(&owner), task_write(&waiting), task_write(&completing)], ctx).await.unwrap();
    assert_eq!(storage.task(waiting_id, ctx).await.unwrap(), Some(waiting.clone()));
    assert_eq!(storage.task(completing_id, ctx).await.unwrap(), Some(completing.clone()));
    let scan = |status| async move {
        let query = TaskQuery { status: Some(status), ..Default::default() };
        storage.scan_tasks(&query, 10, None, ctx).await.unwrap().items
    };
    assert_eq!(scan(TaskStatus::Waiting).await, [waiting]);
    assert_eq!(scan(TaskStatus::Completing).await, std::slice::from_ref(&completing));
    assert_eq!(ids(&scan(TaskStatus::Pending).await, |task| task.id.0), [owner_id.0]);
    let terminal = TaskRecord { state: TaskState::Terminal { outcome }, ..completing };
    storage.commit(&[task_write(&terminal)], ctx).await.unwrap();
    assert_eq!(scan(TaskStatus::Completing).await, []);
    assert_eq!(scan(TaskStatus::Terminal).await, [terminal]);
}

async fn all_submission_ids(storage: &dyn Storage, query: &SubmissionQuery) -> Vec<u64> {
    let mut found = Vec::new();
    let mut cursor: Option<Cursor> = None;
    loop {
        let page = storage.scan_submissions(query, 1, cursor.as_ref(), context()).await.unwrap();
        found.extend(ids(&page.items, |item| item.id.0));
        cursor = page.next;
        if cursor.is_none() {
            return found;
        }
    }
}

pub async fn indexes_request_ids_per_conversation(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let second_conversation: ConversationId = mint(storage).await;
    let record = ConversationRecord { id: second_conversation, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: record }], ctx).await.unwrap();
    let first_id: SubmissionId = mint(storage).await;
    let second_id: SubmissionId = mint(storage).await;
    let other_id: SubmissionId = mint(storage).await;
    let queued = |id, conversation_id, request: &str| SubmissionRecord {
        request_id: Some(request.into()),
        ..submission(id, conversation_id, SubmissionStatus::Queued)
    };
    let first = queued(first_id, root_id, "same");
    let second = queued(second_id, root_id, "other");
    let other = queued(other_id, second_conversation, "same");
    let writes = [submission_write(&first), submission_write(&second), submission_write(&other)];
    storage.commit(&writes, ctx).await.unwrap();
    assert_eq!(storage.submission_by_request(root_id, "same", ctx).await.unwrap(), Some(first));
    assert_eq!(storage.submission_by_request(second_conversation, "same", ctx).await.unwrap(), Some(other));

    let entry: EntryId = mint(storage).await;
    let placed = SubmissionRecord { status: SubmissionStatus::Placed, entry: Some(entry), ..second };
    storage.commit(&[submission_write(&placed)], ctx).await.unwrap();
    assert_eq!(storage.submission(second_id, ctx).await.unwrap(), Some(placed.clone()));
    assert_eq!(storage.submission_by_request(root_id, "other", ctx).await.unwrap(), Some(placed.clone()));

    let query = |conversation_id, status| SubmissionQuery { conversation_id, status, order: None };
    assert_eq!(all_submission_ids(storage, &query(None, None)).await, [first_id.0, second_id.0, other_id.0]);
    assert_eq!(all_submission_ids(storage, &query(Some(root_id), None)).await, [first_id.0, second_id.0]);
    // A status change moves the record between status scans.
    let queued_ids = all_submission_ids(storage, &query(None, Some(SubmissionStatus::Queued))).await;
    assert_eq!(queued_ids, [first_id.0, other_id.0]);
    assert_eq!(all_submission_ids(storage, &query(None, Some(SubmissionStatus::Placed))).await, [second_id.0]);
    let second_queued = query(Some(second_conversation), Some(SubmissionStatus::Queued));
    assert_eq!(all_submission_ids(storage, &second_queued).await, [other_id.0]);
    let second_placed = query(Some(second_conversation), Some(SubmissionStatus::Placed));
    assert_eq!(all_submission_ids(storage, &second_placed).await, Vec::<u64>::new());
    let placed_page = storage.scan_submissions(&query(None, Some(SubmissionStatus::Placed)), 10, None, ctx).await;
    assert_eq!(placed_page.unwrap().items, [placed]);
}

pub async fn stores_passive_write_submissions(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let ctx = context();
    let root_id = create_root(storage).await;
    let done_id: SubmissionId = mint(storage).await;
    let failed_id: SubmissionId = mint(storage).await;
    let passive = |id, request: &str| SubmissionRecord {
        kind: SubmissionType::Write,
        request_id: Some(request.into()),
        ..submission(id, root_id, SubmissionStatus::Queued)
    };
    let queued_done = passive(done_id, "passive-done");
    let queued_failed = passive(failed_id, "passive-failed");
    storage.commit(&[submission_write(&queued_done), submission_write(&queued_failed)], ctx).await.unwrap();

    let entry: EntryId = mint(storage).await;
    let done = SubmissionRecord { status: SubmissionStatus::Done, entry: Some(entry), ..queued_done };
    let unanswered = SubmissionRecord {
        status: SubmissionStatus::Unanswered,
        reason: Some("closed".into()),
        detail: Some(json(r#"{"retryable":false}"#)),
        ..queued_failed
    };
    storage.commit(&[submission_write(&done), submission_write(&unanswered)], ctx).await.unwrap();
    assert_eq!(storage.submission(done_id, ctx).await.unwrap(), Some(done.clone()));
    assert_eq!(storage.submission_by_request(root_id, "passive-done", ctx).await.unwrap(), Some(done));
    assert_eq!(storage.submission(failed_id, ctx).await.unwrap(), Some(unanswered.clone()));
    assert_eq!(storage.submission_by_request(root_id, "passive-failed", ctx).await.unwrap(), Some(unanswered));
}
