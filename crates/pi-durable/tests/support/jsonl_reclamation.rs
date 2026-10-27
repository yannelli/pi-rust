//! Publication flush order and reclamation cases of `jsonl-storage.test.ts`.

use super::*;

fn reclaim_files(directory: &str) -> Vec<String> {
    let names =
        std::fs::read_dir(directory).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned());
    names.filter(|name| name.ends_with(".reclaim")).collect()
}

#[tokio::test]
async fn orders_publication_flushes_and_flushes_main_only_to_authorize_reclamation() {
    for fsync in [false, true] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), fsync).await;
        create_root(&storage).await;
        let first: DocumentId = mint(&storage).await;
        let second: DocumentId = mint(&storage).await;
        fs.clear();
        let writes = [create(session_document(second, "second"), "{}"), create(session_document(first, "first"), "{}")];
        storage.commit(&writes, &|_| {}, ctx()).await.unwrap();
        let mut expected = vec![format!("append:doc-{second}.jsonl"), format!("append:doc-{first}.jsonl")];
        if fsync {
            expected.extend([format!("flush:doc-{second}.jsonl"), format!("flush:doc-{first}.jsonl")]);
        }
        expected.push("append:main.jsonl".into());
        assert_eq!(fs.operations(), expected);

        fs.clear();
        storage.commit(&[base(first, r#"{"checkpoint":true}"#)], &|_| {}, ctx()).await.unwrap();
        let mut expected = vec![format!("append:doc-{first}.jsonl")];
        if fsync {
            expected.push(format!("flush:doc-{first}.jsonl"));
        }
        expected.push("append:main.jsonl".into());
        if fsync {
            expected.push("flush:main.jsonl".into());
        }
        expected.push(format!("write:doc-{first}.jsonl.reclaim"));
        if fsync {
            expected.push(format!("flush:doc-{first}.jsonl.reclaim"));
        }
        expected.push(format!("rename:doc-{first}.jsonl.reclaim->doc-{first}.jsonl"));
        assert_eq!(fs.operations(), expected);

        fs.clear();
        let id: EntryId = mint(&storage).await;
        let entry = EntryRecord {
            kind: "main-only".into(),
            model: None,
            data: None,
            edits: None,
            id,
            conversation_id: ROOT_CONVERSATION_ID,
            head: None,
            by_task_id: None,
        };
        storage.commit(&[StorageWrite::Entry { value: entry }], &|_| {}, ctx()).await.unwrap();
        assert_eq!(fs.operations(), ["append:main.jsonl"]);

        let task_id: TaskId = mint(&storage).await;
        storage.commit(&[task(pending_task(task_id, "ready"))], &|_| {}, ctx()).await.unwrap();
        fs.clear();
        storage.commit(&[task(terminal_task(task_id))], &|_| {}, ctx()).await.unwrap();
        let mut expected = vec!["append:main.jsonl".to_string()];
        if fsync {
            expected.push("flush:main.jsonl".into());
        }
        expected.push(format!("remove:task-{task_id}.jsonl"));
        assert_eq!(fs.operations(), expected);
    }
}

#[tokio::test]
async fn recovers_a_committed_base_across_reclaim_failures() {
    let failures = [
        (Operation::Write, Mode::Before),
        (Operation::Write, Mode::Short),
        (Operation::Write, Mode::After),
        (Operation::Rename, Mode::Before),
        (Operation::Rename, Mode::After),
    ];
    for (operation, mode) in failures {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), false).await;
        create_root(&storage).await;
        let id: DocumentId = mint(&storage).await;
        storage
            .commit(&[create(session_document(id, "test.document"), r#"{"count":0}"#)], &|_| {}, ctx())
            .await
            .unwrap();
        storage.commit(&[delta(id, r#"[["s",["count"],1]]"#)], &|_| {}, ctx()).await.unwrap();
        fs.fail(Failure { operation, call: 1, mode });
        assert_eq!(storage.commit(&[base(id, r#"{"count":2}"#)], &|_| {}, ctx()).await.unwrap(), Seq(4));
        assert_eq!(value(&storage, id).await, Some(json(r#"{"count":2}"#)));
        storage.close(ctx()).await.unwrap();

        let reopened = open(&directory).await;
        assert_eq!(value(&reopened, id).await, Some(json(r#"{"count":2}"#)), "{operation:?} {mode:?}");
        assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 1);
        assert_eq!(reclaim_files(&directory), Vec::<String>::new());
    }
}

#[tokio::test]
async fn recovers_document_retirement_reclamation_across_remove_failures() {
    for mode in [Mode::Before, Mode::After] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), false).await;
        create_root(&storage).await;
        let task_id: TaskId = mint(&storage).await;
        let id: DocumentId = mint(&storage).await;
        let record = DocumentCreate {
            id,
            kind: "task.document".into(),
            key: None,
            scope: DocumentScope::Task { task_id },
            history: None,
            fork: None,
        };
        storage
            .commit(&[task(pending_task(task_id, "ready")), create(record, r#"{"count":1}"#)], &|_| {}, ctx())
            .await
            .unwrap();
        fs.fail(Failure { operation: Operation::Remove, call: 1, mode });
        assert_eq!(storage.commit(&[StorageWrite::DocumentRetire { id }], &|_| {}, ctx()).await.unwrap(), Seq(3));
        assert_eq!(value(&storage, id).await, None);
        storage.close(ctx()).await.unwrap();

        let reopened = open(&directory).await;
        assert_eq!(value(&reopened, id).await, None);
        assert_eq!(reopened.task(task_id, ctx()).await.unwrap(), Some(pending_task(task_id, "ready")));
        assert!(!exists(&file(&directory, &format!("doc-{id}.jsonl"))));
        assert_eq!(reclaim_files(&directory), Vec::<String>::new());
    }
}

#[tokio::test]
async fn defers_reclamation_after_authorizing_main_flush_failure() {
    for mode in [Mode::Before, Mode::After] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), true).await;
        create_root(&storage).await;
        let id: DocumentId = mint(&storage).await;
        storage
            .commit(&[create(session_document(id, "test.document"), r#"{"count":0}"#)], &|_| {}, ctx())
            .await
            .unwrap();
        fs.fail(Failure { operation: Operation::Flush, call: 2, mode });
        assert_eq!(storage.commit(&[base(id, r#"{"count":2}"#)], &|_| {}, ctx()).await.unwrap(), Seq(3));
        let expected = [format!("append:doc-{id}.jsonl"), format!("flush:doc-{id}.jsonl")];
        let expected: Vec<String> =
            expected.into_iter().chain(["append:main.jsonl".into(), "flush:main.jsonl".into()]).collect();
        assert_eq!(fs.operations(), expected);
        assert_eq!(value(&storage, id).await, Some(json(r#"{"count":2}"#)));
        assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 2);
        storage.close(ctx()).await.unwrap();

        let recovery = Arc::new(InstrumentedFs::new(&directory));
        recovery.fail(Failure { operation: Operation::Flush, call: 1, mode });
        let deferred = open_with(&directory, recovery.clone(), true).await;
        assert_eq!(value(&deferred, id).await, Some(json(r#"{"count":2}"#)));
        assert_eq!(recovery.operations(), ["flush:main.jsonl"]);
        assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 2);
        deferred.close(ctx()).await.unwrap();

        let reclaimed = open_with(&directory, local(&directory), true).await;
        assert_eq!(value(&reclaimed, id).await, Some(json(r#"{"count":2}"#)));
        assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 1);
    }
}

#[tokio::test]
async fn keeps_a_committed_base_usable_after_reclaim_temp_flush_failure() {
    for mode in [Mode::Before, Mode::After] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), true).await;
        create_root(&storage).await;
        let id: DocumentId = mint(&storage).await;
        storage
            .commit(&[create(session_document(id, "test.document"), r#"{"count":0}"#)], &|_| {}, ctx())
            .await
            .unwrap();
        fs.fail(Failure { operation: Operation::Flush, call: 3, mode });
        assert_eq!(storage.commit(&[base(id, r#"{"count":2}"#)], &|_| {}, ctx()).await.unwrap(), Seq(3));
        fs.clear();
        storage.commit(&[delta(id, r#"[["s",["count"],3]]"#)], &|_| {}, ctx()).await.unwrap();
        storage.close(ctx()).await.unwrap();

        let reopened = open_with(&directory, local(&directory), true).await;
        assert_eq!(value(&reopened, id).await, Some(json(r#"{"count":3}"#)));
        assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 2);
    }
}

#[tokio::test]
async fn recovers_terminal_task_reclamation_across_remove_failures() {
    for mode in [Mode::Before, Mode::After] {
        let (_guard, directory) = temp_directory();
        let fs = Arc::new(InstrumentedFs::new(&directory));
        let storage = open_with(&directory, fs.clone(), false).await;
        create_root(&storage).await;
        let id: TaskId = mint(&storage).await;
        storage.commit(&[task(pending_task(id, "ready"))], &|_| {}, ctx()).await.unwrap();
        fs.fail(Failure { operation: Operation::Remove, call: 1, mode });
        assert_eq!(storage.commit(&[task(terminal_task(id))], &|_| {}, ctx()).await.unwrap(), Seq(3));
        assert_eq!(storage.task(id, ctx()).await.unwrap(), Some(terminal_task(id)));
        storage.close(ctx()).await.unwrap();

        let reopened = open(&directory).await;
        assert_eq!(reopened.task(id, ctx()).await.unwrap(), Some(terminal_task(id)));
        assert!(!exists(&file(&directory, &format!("task-{id}.jsonl"))));
        assert_eq!(reclaim_files(&directory), Vec::<String>::new());
    }
}

#[tokio::test]
async fn appends_later_deltas_to_the_replacement_sidecar_after_a_current_only_base() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let id: DocumentId = mint(&storage).await;
    storage.commit(&[create(session_document(id, "test.document"), r#"{"count":0}"#)], &|_| {}, ctx()).await.unwrap();
    storage.commit(&[base(id, r#"{"count":10}"#)], &|_| {}, ctx()).await.unwrap();
    storage.commit(&[delta(id, r#"[["s",["count"],11]]"#)], &|_| {}, ctx()).await.unwrap();
    assert_eq!(read_lines(&file(&directory, &format!("doc-{id}.jsonl"))).len(), 2);
    let reopened = open(&directory).await;
    assert_eq!(value(&reopened, id).await, Some(json(r#"{"count":11}"#)));
}
