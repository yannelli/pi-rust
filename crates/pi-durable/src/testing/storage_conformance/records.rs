//! Record identity, atomicity, detachment, and lifecycle cases.

use std::sync::Arc;

use pi_chord::Json;

use super::{context, create_root, entry, expect_err, json, mint, pending_task};
use crate::ids::{ConversationId, EntryId, ROOT_CONVERSATION_ID, SubmissionId, TaskId};
use crate::storage::scan::MAX_SAFE_INTEGER;
use crate::types::{
    ConversationRecord, Storage, StorageWrite, SubmissionRecord, SubmissionStatus, SubmissionType, TaskState,
};

fn root_write() -> StorageWrite {
    StorageWrite::Conversation { value: ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None } }
}

pub(crate) fn submission(
    id: SubmissionId,
    conversation_id: ConversationId,
    status: SubmissionStatus,
) -> SubmissionRecord {
    SubmissionRecord {
        conversation_id,
        kind: SubmissionType::Input,
        request_id: None,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
        id,
        trace: Default::default(),
    }
}

pub async fn reserves_root_conversation_id(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    assert_eq!(storage.mint_id().await.unwrap(), 2);
    assert_eq!(create_root(storage).await, ROOT_CONVERSATION_ID);
    assert_eq!(
        storage.conversation(ROOT_CONVERSATION_ID, context()).await.unwrap(),
        Some(ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None })
    );
    expect_err(
        storage.commit(&[root_write()], &|_| {}, context()).await,
        &format!("ID {ROOT_CONVERSATION_ID} already belongs to conversation"),
    );
}

pub async fn commits_mixed_table_writes_atomically(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let task_id: TaskId = mint(storage).await;
    let submission_id: SubmissionId = mint(storage).await;
    let task = pending_task(task_id, root_id, "ready");
    let input = SubmissionRecord {
        request_id: Some("request-1".into()),
        entry: Some(entry_id),
        ..submission(submission_id, root_id, SubmissionStatus::Placed)
    };
    let user_entry =
        crate::types::EntryRecord { data: Some(json(r#"{"text":"hello"}"#)), ..entry(entry_id, root_id, "user") };
    let initial_seq = storage
        .commit(
            &[
                StorageWrite::Entry { value: user_entry.clone() },
                StorageWrite::Task { value: task.clone() },
                StorageWrite::Submission { value: input.clone() },
            ],
            &|_| {},
            context(),
        )
        .await
        .unwrap();

    let stored = storage.entry(entry_id, context()).await.unwrap().unwrap();
    assert_eq!((stored.entry, stored.commit_seq), (user_entry, initial_seq));
    assert_eq!(storage.task(task_id, context()).await.unwrap(), Some(task.clone()));
    assert_eq!(storage.submission(submission_id, context()).await.unwrap(), Some(input.clone()));

    let transient_entry_id: EntryId = mint(storage).await;
    let running = crate::types::TaskRecord {
        state: TaskState::Running { checkpoint: json(r#"{"phase":"effect"}"#) },
        ..task.clone()
    };
    let done = SubmissionRecord { status: SubmissionStatus::Done, answer: Some(transient_entry_id), ..input.clone() };
    let failed = storage
        .commit(
            &[
                StorageWrite::Task { value: running },
                StorageWrite::Submission { value: done },
                StorageWrite::Entry { value: entry(transient_entry_id, root_id, "assistant") },
                StorageWrite::Conversation { value: ConversationRecord { id: root_id, parent: None, owner: None } },
            ],
            &|_| {},
            context(),
        )
        .await;
    expect_err(failed, &format!("ID {root_id} already belongs to conversation"));

    assert_eq!(storage.task(task_id, context()).await.unwrap(), Some(task));
    assert_eq!(storage.submission(submission_id, context()).await.unwrap(), Some(input));
    assert_eq!(storage.entry(transient_entry_id, context()).await.unwrap(), None);
    let after_id: EntryId = mint(storage).await;
    let after_rollback_seq = storage
        .commit(&[StorageWrite::Entry { value: entry(after_id, root_id, "after-rollback") }], &|_| {}, context())
        .await
        .unwrap();
    assert!(after_rollback_seq > initial_seq);
}

pub async fn detaches_writes_and_returned_records(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let task_id: TaskId = mint(storage).await;
    let submission_id: SubmissionId = mint(storage).await;
    let mut entry_data = json(r#"{"nested":[1,2]}"#);
    let mut checkpoint = json(r#"{"phase":"ready","nested":{"count":1}}"#);
    let mut detail = json(r#"{"codes":["initial"]}"#);
    let stored_entry = crate::types::EntryRecord { data: Some(entry_data.clone()), ..entry(entry_id, root_id, "note") };
    let stored_task = crate::types::TaskRecord {
        state: TaskState::Pending { checkpoint: checkpoint.clone() },
        ..pending_task(task_id, root_id, "ready")
    };
    let stored_input = SubmissionRecord {
        reason: Some("failed".into()),
        detail: Some(detail.clone()),
        ..submission(submission_id, root_id, SubmissionStatus::Unanswered)
    };
    storage
        .commit(
            &[
                StorageWrite::Entry { value: stored_entry },
                StorageWrite::Task { value: stored_task },
                StorageWrite::Submission { value: stored_input },
            ],
            &|_| {},
            context(),
        )
        .await
        .unwrap();

    entry_data.get_mut("nested").unwrap().push(3).unwrap();
    checkpoint.get_mut("nested").unwrap().set("count", 2).unwrap();
    detail.get_mut("codes").unwrap().push("mutated").unwrap();
    let expect_unchanged = || async {
        let read_entry = storage.entry(entry_id, context()).await.unwrap().unwrap().entry;
        assert_eq!(read_entry.data, Some(json(r#"{"nested":[1,2]}"#)));
        let read_task = storage.task(task_id, context()).await.unwrap().unwrap();
        let expected = TaskState::Pending { checkpoint: json(r#"{"phase":"ready","nested":{"count":1}}"#) };
        assert_eq!(read_task.state, expected);
        let read_input = storage.submission(submission_id, context()).await.unwrap().unwrap();
        assert_eq!(read_input.detail, Some(json(r#"{"codes":["initial"]}"#)));
    };
    expect_unchanged().await;

    let mut read_entry = storage.entry(entry_id, context()).await.unwrap().unwrap().entry;
    read_entry.data.as_mut().unwrap().get_mut("nested").unwrap().push(9).unwrap();
    let mut read_task = storage.task(task_id, context()).await.unwrap().unwrap();
    if let TaskState::Pending { checkpoint } = &mut read_task.state {
        checkpoint.get_mut("nested").unwrap().set("count", 9).unwrap();
    }
    let mut read_input = storage.submission(submission_id, context()).await.unwrap().unwrap();
    read_input.detail.as_mut().unwrap().get_mut("codes").unwrap().push("read mutation").unwrap();
    expect_unchanged().await;
}

/// Rust maps have no prototypes; the case checks that such keys are kept as plain data and stay detached.
pub async fn detaches_prototype_like_keys(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let entry_id: EntryId = mint(storage).await;
    let text = r#"{"__proto__":{"polluted":false},"constructor":{"label":"stored"},"toString":"value"}"#;
    let mut data = json(text);
    let note = crate::types::EntryRecord { data: Some(data.clone()), ..entry(entry_id, root_id, "note") };
    storage.commit(&[StorageWrite::Entry { value: note }], &|_| {}, context()).await.unwrap();

    data.get_mut("__proto__").unwrap().set("polluted", true).unwrap();
    data.get_mut("constructor").unwrap().set("label", "mutated").unwrap();
    let mut first_read = storage.entry(entry_id, context()).await.unwrap().unwrap().entry.data.unwrap();
    assert_eq!(first_read, json(text));
    assert_eq!(first_read.to_string(), text);
    first_read.get_mut("__proto__").unwrap().set("polluted", true).unwrap();
    let second_read = storage.entry(entry_id, context()).await.unwrap().unwrap().entry.data.unwrap();
    assert_eq!(second_read["__proto__"], json(r#"{"polluted":false}"#));
    assert_eq!(second_read["constructor"], json(r#"{"label":"stored"}"#));
    assert_eq!(second_read["toString"], Json::from("value"));
}

pub async fn keeps_one_global_id_namespace(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    let root_id = create_root(storage).await;
    let explicit_entry_id = EntryId(100);
    storage
        .commit(&[StorageWrite::Entry { value: entry(explicit_entry_id, root_id, "message") }], &|_| {}, context())
        .await
        .unwrap();
    assert_eq!(storage.mint_id().await.unwrap(), 101);
    let task = pending_task(TaskId(explicit_entry_id.0), root_id, "ready");
    expect_err(
        storage.commit(&[StorageWrite::Task { value: task }], &|_| {}, context()).await,
        &format!("ID {explicit_entry_id} already belongs to entry"),
    );

    let last = entry(EntryId(MAX_SAFE_INTEGER), root_id, "last-id");
    storage.commit(&[StorageWrite::Entry { value: last }], &|_| {}, context()).await.unwrap();
    expect_err(storage.mint_id().await, "ID space is exhausted");
    expect_err(storage.mint_id().await, "ID space is exhausted");
}

pub async fn rejects_operations_after_close(storage: Arc<dyn Storage>) {
    let storage = &*storage;
    create_root(storage).await;
    storage.close(context()).await.unwrap();
    expect_err(storage.conversation(ROOT_CONVERSATION_ID, context()).await, "closed");
    expect_err(storage.commit(&[], &|_| {}, context()).await, "closed");
    expect_err(storage.mint_id().await, "closed");
}
