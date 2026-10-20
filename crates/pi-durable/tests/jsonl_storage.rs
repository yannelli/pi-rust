//! Port of upstream `test/jsonl-storage.test.ts`.

mod support;

use std::path::Path;
use std::sync::Arc;

use pi_chord::{BACKGROUND_CONTEXT, Context, Json};
use pi_durable::env::{FileSystem, LocalEnvOptions, LocalExecutionEnv};
use pi_durable::storage::jsonl::{JsonlStorage, JsonlStorageOptions, open_local_jsonl_storage};
use pi_durable::testing::storage_conformance::StorageTest;
use pi_durable::*;
use support::instrumented_fs::{Failure, InstrumentedFs, Mode, Operation};
use support::reopening::{Reopen, ReopeningStorage};

fn ctx() -> &'static Context {
    &BACKGROUND_CONTEXT
}

fn local(directory: &str) -> Arc<dyn FileSystem> {
    Arc::new(LocalExecutionEnv::new(LocalEnvOptions::new(directory)))
}

fn temp_directory() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().to_string_lossy().into_owned();
    (directory, path)
}

async fn open_with(directory: &str, fs: Arc<dyn FileSystem>, fsync: bool) -> JsonlStorage {
    JsonlStorage::open(directory, fs, ctx(), JsonlStorageOptions { fsync }).await.unwrap()
}

async fn open(directory: &str) -> JsonlStorage {
    open_with(directory, local(directory), false).await
}

async fn open_err(directory: &str) -> String {
    let opened = JsonlStorage::open(directory, local(directory), ctx(), JsonlStorageOptions::default()).await;
    opened.err().expect("open must fail").to_string()
}

async fn with_jsonl_storage(test: StorageTest) {
    let (_guard, directory) = temp_directory();
    test(Arc::new(open(&directory).await)).await;
}

async fn with_reopening_jsonl_storage(test: StorageTest) {
    let (_guard, directory) = temp_directory();
    let reopen: Reopen = Arc::new(move || {
        let directory = directory.clone();
        Box::pin(async move { Arc::new(open(&directory).await) as Arc<dyn Storage> })
    });
    let storage = Arc::new(ReopeningStorage::new(reopen).await);
    test(storage.clone()).await;
    storage.close(ctx()).await.unwrap();
}

mod conformance {
    pi_durable::storage_conformance_tests!(super::with_jsonl_storage);
}

mod conformance_across_reopen {
    pi_durable::storage_conformance_tests!(super::with_reopening_jsonl_storage);
}

fn json(text: &str) -> Json {
    Json::parse(text).unwrap()
}

fn pending_task(id: TaskId, phase: &str) -> TaskRecord {
    TaskRecord {
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "test.task".into(),
        version: 1,
        input: Json::Null,
        owner: None,
        background: false,
        abort_requested: false,
        abort_reason: None,
        abandon_on_restart: None,
        state: TaskState::Pending { checkpoint: Json::from(serde_json::json!({ "phase": phase })) },
        memos: None,
        started_at: None,
        ended_at: None,
        restarts: None,
        trace: Default::default(),
        links: Default::default(),
    }
}

fn terminal_task(id: TaskId) -> TaskRecord {
    let outcome = TaskOutcome::Completed { result: Some(Json::Null) };
    TaskRecord { state: TaskState::Terminal { outcome }, ..pending_task(id, "ready") }
}

fn task(value: TaskRecord) -> StorageWrite {
    StorageWrite::Task { value }
}

async fn create_root(storage: &dyn Storage) {
    let root = ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None };
    storage.commit(&[StorageWrite::Conversation { value: root }], ctx()).await.unwrap();
}

fn session_document(id: DocumentId, kind: &str) -> DocumentCreate {
    DocumentCreate { id, kind: kind.into(), key: None, scope: DocumentScope::Session, history: None, fork: None }
}

fn create(record: DocumentCreate, value: &str) -> StorageWrite {
    StorageWrite::DocumentCreate { record, content: DocumentContent::Base { version: 1, value: json(value) } }
}

fn base(id: DocumentId, value: &str) -> StorageWrite {
    StorageWrite::DocumentChange { id, content: DocumentContent::Base { version: 1, value: json(value) } }
}

fn delta(id: DocumentId, ops: &str) -> StorageWrite {
    let ops = pi_chord::delta::parse_ops(&json(ops)).unwrap();
    StorageWrite::DocumentChange { id, content: DocumentContent::Delta { version: 1, ops } }
}

fn file(directory: &str, name: &str) -> String {
    Path::new(directory).join(name).to_string_lossy().into_owned()
}

fn read_lines(path: &str) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    if text.is_empty() { Vec::new() } else { text.trim_end().split('\n').map(String::from).collect() }
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

async fn mint<T: From<u64>>(storage: &dyn Storage) -> T {
    T::from(storage.mint_id().await.unwrap())
}

async fn value(storage: &dyn Storage, id: DocumentId) -> Option<Json> {
    storage.document(id, DocumentPoint::Current, ctx()).await.unwrap().map(|stored| stored.value)
}

#[tokio::test]
async fn opens_through_the_local_adapter() {
    let (_guard, directory) = temp_directory();
    let storage = open_local_jsonl_storage(&directory, ctx(), JsonlStorageOptions::default()).await.unwrap();
    create_root(&storage).await;
    let root = storage.conversation(ROOT_CONVERSATION_ID, ctx()).await.unwrap();
    assert_eq!(root, Some(ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None }));
}

#[tokio::test]
async fn publishes_every_commit_before_reclaiming_sidecars() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let task_id: TaskId = mint(&storage).await;
    let document_id: DocumentId = mint(&storage).await;
    storage.commit(&[task(pending_task(task_id, "ready"))], ctx()).await.unwrap();
    storage.commit(&[create(session_document(document_id, "test.document"), r#"{"count":0}"#)], ctx()).await.unwrap();
    storage.commit(&[delta(document_id, "[]")], ctx()).await.unwrap();
    storage.commit(&[base(document_id, r#"{"count":1}"#)], ctx()).await.unwrap();
    assert_eq!(read_lines(&file(&directory, &format!("task-{task_id}.jsonl"))).len(), 1);
    assert_eq!(read_lines(&file(&directory, &format!("doc-{document_id}.jsonl"))).len(), 1);

    storage.commit(&[StorageWrite::DocumentRetire { id: document_id }], ctx()).await.unwrap();
    storage.commit(&[task(terminal_task(task_id))], ctx()).await.unwrap();
    let main = read_lines(&file(&directory, "main.jsonl"));
    assert_eq!(main.len(), 7);
    assert!(!exists(&file(&directory, &format!("task-{task_id}.jsonl"))));
    assert!(!exists(&file(&directory, &format!("doc-{document_id}.jsonl"))));
    assert!(main.iter().all(|line| json(line)["type"] == Json::from("commit")));
}

#[tokio::test]
async fn orders_multiple_live_task_replacements_by_commit_ordinal() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let task_id: TaskId = mint(&storage).await;
    let writes = [task(pending_task(task_id, "first")), task(pending_task(task_id, "second"))];
    storage.commit(&writes, ctx()).await.unwrap();
    assert_eq!(read_lines(&file(&directory, &format!("task-{task_id}.jsonl"))).len(), 2);
    let reopened = open(&directory).await;
    assert_eq!(reopened.task(task_id, ctx()).await.unwrap(), Some(pending_task(task_id, "second")));
}

#[tokio::test]
async fn removes_a_complete_plus_torn_unconfirmed_sidecar_append() {
    let (_guard, directory) = temp_directory();
    let fs = Arc::new(InstrumentedFs::new(&directory));
    let storage = open_with(&directory, fs.clone(), false).await;
    create_root(&storage).await;
    let task_id: TaskId = mint(&storage).await;
    fs.fail(Failure { operation: Operation::Append, call: 1, mode: Mode::Short });
    let writes =
        [task(pending_task(task_id, "first")), task(pending_task(task_id, &format!("second-{}", "x".repeat(512))))];
    let error = storage.commit(&writes, ctx()).await.unwrap_err();
    assert!(error.to_string().contains("poisoned"), "{error}");
    let partial = std::fs::read(file(&directory, &format!("task-{task_id}.jsonl"))).unwrap();
    assert_ne!(partial.last(), Some(&b'\n'));
    assert_eq!(partial.iter().filter(|byte| **byte == b'\n').count(), 1);

    let reopened = open(&directory).await;
    assert_eq!(reopened.task(task_id, ctx()).await.unwrap(), None);
    assert_eq!(std::fs::metadata(file(&directory, &format!("task-{task_id}.jsonl"))).unwrap().len(), 0);
}

/// Upstream commits an entry holding a `bigint`, which JSON cannot encode. Rust values always encode, so a batch that
/// fails preparation (a duplicate root) shows the same: no I/O happens and the storage stays usable.
#[tokio::test]
async fn serializes_the_candidate_before_io_and_leaves_preparation_failures_usable() {
    let (_guard, directory) = temp_directory();
    let fs = Arc::new(InstrumentedFs::new(&directory));
    let storage = open_with(&directory, fs.clone(), false).await;
    create_root(&storage).await;
    fs.clear();
    let id: EntryId = mint(&storage).await;
    let entry = |kind: &str| EntryRecord {
        kind: kind.into(),
        model: None,
        data: None,
        edits: None,
        id,
        conversation_id: ROOT_CONVERSATION_ID,
        head: None,
        by_task_id: None,
    };
    let duplicate = StorageWrite::Conversation {
        value: ConversationRecord { id: ROOT_CONVERSATION_ID, parent: None, owner: None },
    };
    assert!(storage.commit(&[StorageWrite::Entry { value: entry("bad") }, duplicate], ctx()).await.is_err());
    assert_eq!(fs.operations(), Vec::<String>::new());
    assert_eq!(storage.entry(id, ctx()).await.unwrap(), None);
    assert_eq!(storage.commit(&[StorageWrite::Entry { value: entry("good") }], ctx()).await.unwrap(), Seq(2));
}

#[tokio::test]
async fn poisons_after_append_failures_and_recovers_only_confirmed_state() {
    let failures = [(1, Mode::Before), (1, Mode::After), (1, Mode::Short), (2, Mode::Before), (2, Mode::After)];
    let failures = failures.into_iter().chain([(3, Mode::Before), (3, Mode::Short), (3, Mode::After)]);
    for (call, mode) in failures {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), false).await;
        create_root(&storage).await;
        let first: DocumentId = mint(&storage).await;
        let second: DocumentId = mint(&storage).await;
        fs.fail(Failure { operation: Operation::Append, call, mode });
        let writes = [
            create(session_document(first, "first"), r#"{"text":"α"}"#),
            create(session_document(second, "second"), r#"{"text":"β"}"#),
        ];
        assert!(storage.commit(&writes, ctx()).await.unwrap_err().to_string().contains("poisoned"));
        let read = storage.document(first, DocumentPoint::Current, ctx()).await;
        assert!(read.unwrap_err().to_string().contains("poisoned"));

        let reopened = open(&directory).await;
        let survived = call == 3 && mode == Mode::After;
        let expect = |text: &str| survived.then(|| json(&format!(r#"{{"text":"{text}"}}"#)));
        assert_eq!(value(&reopened, first).await, expect("α"), "append {call} {mode:?}");
        assert_eq!(value(&reopened, second).await, expect("β"), "append {call} {mode:?}");
    }
}

#[tokio::test]
async fn poisons_after_flush_failures_and_never_writes_a_marker() {
    for (call, mode) in [(1, Mode::Before), (1, Mode::After), (2, Mode::Before), (2, Mode::After)] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), true).await;
        create_root(&storage).await;
        let first: DocumentId = mint(&storage).await;
        let second: DocumentId = mint(&storage).await;
        fs.fail(Failure { operation: Operation::Flush, call, mode });
        let writes = [
            create(session_document(first, "flush.first"), "{}"),
            create(session_document(second, "flush.second"), "{}"),
        ];
        assert!(storage.commit(&writes, ctx()).await.unwrap_err().to_string().contains("poisoned"));
        let reopened = open(&directory).await;
        assert_eq!(value(&reopened, first).await, None);
        assert_eq!(value(&reopened, second).await, None);
    }
}

#[path = "support/jsonl_reclamation.rs"]
mod reclamation;

#[path = "support/jsonl_recovery.rs"]
mod recovery;
