//! History, torn tails, and corruption cases of `jsonl-storage.test.ts`.

use std::io::Write;

use super::*;

fn append_bytes(path: &str, bytes: &[u8]) {
    std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap().write_all(bytes).unwrap();
}

#[tokio::test]
async fn never_reclaims_rewindable_document_history() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let id: DocumentId = mint(&storage).await;
    let record = DocumentCreate {
        id,
        kind: "rewindable".into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    };
    let created_at = storage.commit(&[create(record, r#"{"count":0}"#)], &|_| {}, ctx()).await.unwrap();
    let changed_at = storage.commit(&[delta(id, r#"[["s",["count"],1]]"#)], &|_| {}, ctx()).await.unwrap();
    storage.commit(&[base(id, r#"{"count":2}"#)], &|_| {}, ctx()).await.unwrap();
    storage.commit(&[StorageWrite::DocumentRetire { id }], &|_| {}, ctx()).await.unwrap();
    let path = file(&directory, &format!("doc-{id}.jsonl"));
    assert_eq!(read_lines(&path).len(), 3);
    let reopened = &open(&directory).await;
    let at = |seq| async move { reopened.document(id, DocumentPoint::At(seq), ctx()).await.unwrap().map(|d| d.value) };
    assert_eq!(at(created_at).await, Some(json(r#"{"count":0}"#)));
    assert_eq!(at(changed_at).await, Some(json(r#"{"count":1}"#)));
    assert_eq!(value(reopened, id).await, None);
    assert_eq!(read_lines(&path).len(), 3);
}

#[tokio::test]
async fn reclaims_retired_task_session_and_latest_conversation_sidecars() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let task_id: TaskId = mint(&storage).await;
    let session_id: DocumentId = mint(&storage).await;
    let latest_id: DocumentId = mint(&storage).await;
    let task_document_id: DocumentId = mint(&storage).await;
    let latest = DocumentCreate {
        id: latest_id,
        kind: "latest".into(),
        key: None,
        scope: DocumentScope::Conversation { conversation_id: ROOT_CONVERSATION_ID },
        history: Some(DocumentHistory::Latest),
        fork: Some(DocumentFork::Current),
    };
    let task_document = DocumentCreate {
        id: task_document_id,
        kind: "task".into(),
        key: None,
        scope: DocumentScope::Task { task_id },
        history: None,
        fork: None,
    };
    let writes = [
        task(pending_task(task_id, "ready")),
        create(session_document(session_id, "session"), "{}"),
        create(latest, "{}"),
        create(task_document, "{}"),
    ];
    let created_at = storage.commit(&writes, &|_| {}, ctx()).await.unwrap();
    let writes = [
        StorageWrite::DocumentRetire { id: session_id },
        StorageWrite::DocumentRetire { id: latest_id },
        StorageWrite::DocumentRetire { id: task_document_id },
        task(terminal_task(task_id)),
    ];
    let retired_at = storage.commit(&writes, &|_| {}, ctx()).await.unwrap();
    for name in [
        format!("doc-{session_id}.jsonl"),
        format!("doc-{latest_id}.jsonl"),
        format!("doc-{task_document_id}.jsonl"),
        format!("task-{task_id}.jsonl"),
    ] {
        assert!(!exists(&file(&directory, &name)), "{name}");
    }

    let reopened = open(&directory).await;
    assert_eq!(reopened.task(task_id, ctx()).await.unwrap(), Some(terminal_task(task_id)));
    for id in [session_id, latest_id, task_document_id] {
        assert_eq!(value(&reopened, id).await, None);
    }
    let address = DocumentAddress { kind: "session".into(), scope: DocumentScope::Session, key: None };
    let found = reopened.find_document(&address, DocumentPoint::At(created_at), ctx()).await.unwrap().unwrap();
    assert_eq!((found.id, found.created_at, found.retired_at), (session_id, created_at, Some(retired_at)));
    assert_eq!(reopened.find_document(&address, DocumentPoint::At(retired_at), ctx()).await.unwrap(), None);
}

#[tokio::test]
async fn truncates_torn_utf8_tails_at_exact_byte_offsets_and_reuses_the_sequence() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let document_id: DocumentId = mint(&storage).await;
    storage
        .commit(&[create(session_document(document_id, "test.document"), r#"{"text":"kept"}"#)], &|_| {}, ctx())
        .await
        .unwrap();
    let sidecar = file(&directory, &format!("doc-{document_id}.jsonl"));
    let main = file(&directory, "main.jsonl");
    let sidecar_size = std::fs::metadata(&sidecar).unwrap().len();
    let main_size = std::fs::metadata(&main).unwrap().len();
    let torn = "{\"text\":\"\u{20ac}".as_bytes();
    append_bytes(&sidecar, &torn[..torn.len() - 1]);
    append_bytes(&main, &torn[..torn.len() - 1]);

    let reopened = open(&directory).await;
    assert_eq!(std::fs::metadata(&sidecar).unwrap().len(), sidecar_size);
    assert_eq!(std::fs::metadata(&main).unwrap().len(), main_size);
    assert_eq!(reopened.commit(&[delta(document_id, "[]")], &|_| {}, ctx()).await.unwrap(), Seq(3));
}

#[tokio::test]
async fn removes_complete_unconfirmed_sidecar_tails_without_resurrecting_terminal_tasks() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let task_id: TaskId = mint(&storage).await;
    storage.commit(&[task(pending_task(task_id, "ready"))], &|_| {}, ctx()).await.unwrap();
    storage.commit(&[task(terminal_task(task_id))], &|_| {}, ctx()).await.unwrap();
    let sidecar = file(&directory, &format!("task-{task_id}.jsonl"));
    assert!(!exists(&sidecar));
    let stale = pi_chord::copy_json(&pending_task(task_id, "stale"), Default::default()).unwrap();
    let record =
        format!(r#"{{"format":1,"type":"record","seq":4,"ordinal":0,"payload":{{"type":"task","value":{stale}}}}}"#);
    append_bytes(&sidecar, format!("{record}\n").as_bytes());

    let reopened = open(&directory).await;
    assert!(!exists(&sidecar));
    assert_eq!(reopened.task(task_id, ctx()).await.unwrap(), Some(terminal_task(task_id)));
    assert_eq!(reopened.commit(&[], &|_| {}, ctx()).await.unwrap(), Seq(4));
}

#[tokio::test]
async fn fails_open_when_confirmed_sidecar_data_is_missing() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let document_id: DocumentId = mint(&storage).await;
    storage.commit(&[create(session_document(document_id, "test.document"), "{}")], &|_| {}, ctx()).await.unwrap();
    std::fs::write(file(&directory, &format!("doc-{document_id}.jsonl")), "").unwrap();
    assert!(open_err(&directory).await.contains("Missing confirmed sidecar record"));
}

#[tokio::test]
async fn rejects_a_confirmed_record_after_an_unconfirmed_sidecar_record() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let document_id: DocumentId = mint(&storage).await;
    storage
        .commit(&[create(session_document(document_id, "test.document"), r#"{"count":0}"#)], &|_| {}, ctx())
        .await
        .unwrap();
    storage.commit(&[delta(document_id, r#"[["s",["count"],1]]"#)], &|_| {}, ctx()).await.unwrap();
    let path = file(&directory, &format!("doc-{document_id}.jsonl"));
    let lines = read_lines(&path);
    let unconfirmed = format!(
        r#"{{"format":1,"type":"record","seq":2,"ordinal":999,"payload":{{"type":"document","id":{document_id},"content":{{"kind":"delta","version":1,"ops":[]}}}}}}"#
    );
    std::fs::write(&path, format!("{}\n{unconfirmed}\n{}\n", lines[0], lines[1])).unwrap();
    assert!(open_err(&directory).await.contains("Confirmed record follows an unconfirmed tail"));
}

#[tokio::test]
async fn rejects_non_increasing_main_commit_sequences() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let path = file(&directory, "main.jsonl");
    append_bytes(&path, &std::fs::read(&path).unwrap());
    assert!(open_err(&directory).await.contains("Commit sequence does not strictly increase"));
}

#[tokio::test]
async fn rejects_structurally_invalid_confirmed_document_content() {
    let (_guard, directory) = temp_directory();
    let storage = open(&directory).await;
    create_root(&storage).await;
    let document_id: DocumentId = mint(&storage).await;
    storage.commit(&[create(session_document(document_id, "test.document"), "{}")], &|_| {}, ctx()).await.unwrap();
    let path = file(&directory, &format!("doc-{document_id}.jsonl"));
    let mut record = json(std::fs::read_to_string(&path).unwrap().trim());
    record.get_mut("payload").unwrap().get_mut("content").unwrap().remove("value");
    std::fs::write(&path, format!("{record}\n")).unwrap();
    assert!(open_err(&directory).await.contains("Invalid document content"));
}

#[tokio::test]
async fn rejects_malformed_complete_main_and_sidecar_lines() {
    let (_main_guard, main_directory) = temp_directory();
    let storage = open(&main_directory).await;
    create_root(&storage).await;
    append_bytes(&file(&main_directory, "main.jsonl"), b"{bad}\n");
    assert!(open_err(&main_directory).await.contains("Malformed complete main.jsonl"));

    let (_sidecar_guard, sidecar_directory) = temp_directory();
    let storage = open(&sidecar_directory).await;
    create_root(&storage).await;
    std::fs::write(file(&sidecar_directory, "doc-99.jsonl"), "{bad}\n").unwrap();
    assert!(open_err(&sidecar_directory).await.contains("Malformed complete doc-99.jsonl"));
}
