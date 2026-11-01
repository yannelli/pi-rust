//! Table reads and writes, task records, and task documents.

use pi_chord::Json;
use pi_durable::session::TransactionScope;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;

use super::support::*;
use super::*;

#[tokio::test]
async fn allows_table_reads_only_before_the_first_table_write() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    test.session
        .commit(
            move |tx| async move {
                let record = ConversationRecord { id: conversation_id, parent: None, owner: None };
                assert_eq!(tx.conversation(conversation_id).await?, Some(record));
                let page = tx.scan_conversations(ConversationQuery::default(), 1, None).await?;
                assert_eq!(page, Page::new(vec![record], None));
                let query = TaskQuery { conversation_id: Some(conversation_id), ..TaskQuery::default() };
                assert!(tx.scan_tasks(query, 10, None).await?.items.is_empty());
                assert!(tx.scan_entries(EntryQuery::new(conversation_id), 10, None).await?.items.is_empty());
                tx.append_entry(conversation_id, EntryDraft::new("note")).await?;
                assert_eq!(tx.conversation(conversation_id).await.unwrap_err().name(), "ReadAfterWrite");
                let task = tx.task(TaskId(1)).await.unwrap_err();
                assert_eq!(task.to_string(), "Tx.task() cannot read tables after the first table write");
                assert_eq!(tx.entry(EntryId(1)).await.unwrap_err().name(), "ReadAfterWrite");
                let scan = tx.scan_conversations(ConversationQuery::default(), 10, None).await;
                assert_eq!(scan.unwrap_err().name(), "ReadAfterWrite");
                let scan = tx.scan_entries(EntryQuery::new(conversation_id), 10, None).await;
                assert_eq!(scan.unwrap_err().name(), "ReadAfterWrite");
                // Document access remains available after table writes.
                tx.doc(&NOTES_DOC, conversation_id).await?.edit(|notes| notes.text = "after write".into())
            },
            &context(),
        )
        .await
        .unwrap();
    let notes = test.session.snapshot(&NOTES_DOC, conversation_id, &context()).await.unwrap();
    assert_eq!(notes.unwrap(), json(r#"{"text":"after write"}"#));
}

#[tokio::test]
async fn passes_caller_selected_limits_and_cursors_through_table_scans() {
    let test = open_test_session();
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(create_conversation(&test.session).await);
    }
    test.session
        .commit(
            move |tx| async move {
                let first = tx.scan_conversations(ConversationQuery::default(), 2, None).await?;
                assert_eq!(first.items.iter().map(|c| c.id).collect::<Vec<_>>(), ids[..2]);
                assert!(first.next.is_some());
                let second = tx.scan_conversations(ConversationQuery::default(), 2, first.next).await?;
                assert_eq!(second.items.iter().map(|c| c.id).collect::<Vec<_>>(), ids[2..]);
                assert!(second.next.is_none());
                Ok(())
            },
            &context(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn treats_synchronous_set_task_as_the_first_table_write() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let task_id = create_task(&test.session, conversation_id, false).await;
    test.session
        .commit_with(
            move |tx| async move {
                let task = tx.task(task_id).await?.unwrap();
                tx.set_task(task)?;
                assert_eq!(tx.task(task_id).await.unwrap_err().name(), "ReadAfterWrite");
                Ok(())
            },
            &context(),
            TransactionScope::default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn creates_conversations_entries_and_tasks_with_minted_ids() {
    let test = open_test_session();
    let (conversation, first, headed, task) = test
        .session
        .commit(
            |tx| async move {
                let conversation = tx.create_conversation(ConversationOwnership::Ownerless).await?;
                let draft = EntryDraft { data: Some(Json::from("one")), ..EntryDraft::new("note") };
                let first = tx.append_entry(conversation.id, draft).await?;
                let draft = EntryDraft { head: Some(DraftHead::SelfEntry), ..EntryDraft::new("summary") };
                let headed = tx.append_entry(conversation.id, draft).await?;
                let options = TaskOptions { background: true, ..TaskOptions::conversation(conversation.id) };
                let task = tx.create_task(work(), json(r#"{"path":"x"}"#), options).await?;
                Ok((conversation, first, headed, task))
            },
            &context(),
        )
        .await
        .unwrap();
    let ids = std::collections::HashSet::from([conversation.id.0, first.id.0, headed.id.0, task.0]);
    assert_eq!(ids.len(), 4);
    assert_eq!(headed.head, Some(headed.id));
    let expected = EntryRecord {
        kind: "note".into(),
        model: None,
        data: Some(Json::from("one")),
        edits: None,
        id: first.id,
        conversation_id: conversation.id,
        head: None,
        by_task_id: None,
    };
    assert_eq!(first, expected);
    let ctx = context();
    assert_eq!(test.storage.entry(headed.id, &ctx).await.unwrap().unwrap().entry, headed);
    let mut stored_task = pending(task, conversation.id, "x", false);
    stored_task.background = true;
    assert_eq!(test.storage.task(task, &ctx).await.unwrap(), Some(stored_task));
    flush().await;
    let changes = test.last_publication().changes;
    assert_eq!(changes.len(), 4);
    let kinds: Vec<_> = changes
        .iter()
        .map(|change| match change {
            CommitChange::Conversation(record) => {
                assert_eq!(*record, conversation);
                "conversation"
            }
            CommitChange::Entry(entry) => {
                assert!(*entry == first || *entry == headed);
                "entry"
            }
            CommitChange::Task(_) => "task",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["conversation", "entry", "entry", "task"]);
    let missing = test
        .session
        .commit(
            |tx| async move {
                let options = TaskOptions { conversation_id: None, ..TaskOptions::conversation(ConversationId(0)) };
                tx.create_task(work(), json(r#"{"path":"x"}"#), options).await
            },
            &ctx,
        )
        .await;
    assert!(message(missing).contains("requires options.conversationId"));
    let absent = test
        .session
        .commit(|tx| async move { tx.append_entry(ConversationId(12345), EntryDraft::new("note")).await }, &ctx)
        .await;
    assert!(message(absent).contains("Conversation 12345 does not exist"));
}

#[tokio::test]
async fn replaces_task_records_completely() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let task_id = create_task(&test.session, conversation_id, false).await;
    replace_task(&test.session, task_id, |task| {
        let mut memos = pi_chord::JsonMap::new();
        memos.insert("choice", "b");
        TaskRecord {
            state: TaskState::Running { checkpoint: json(r#"{"phase":"next","step":2}"#) },
            memos: Some(memos),
            ..task.clone()
        }
    })
    .await
    .unwrap();
    let stored = test.storage.task(task_id, &context()).await.unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Running { checkpoint: json(r#"{"phase":"next","step":2}"#) });
    assert_eq!(stored.memos.unwrap().get("choice"), Some(&Json::from("b")));
}

#[tokio::test]
async fn creates_a_task_and_then_its_document_in_one_transaction_without_read_after_write() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let task_id = test
        .session
        .commit(
            move |tx| async move {
                let options = TaskOptions::conversation(conversation_id);
                let task_id = tx.create_task(work(), json(r#"{"path":"a"}"#), options).await?;
                // Validation reads the candidate task record, which does not count as a table read.
                tx.doc(&PROGRESS_DOC, task_id).await?.edit(|p| p.lines.push("created".into()))?;
                tx.family_doc(&STEP_DOC, task_id, "one", &()).await?.edit(|p| p.lines.push("step".into()))?;
                Ok(task_id)
            },
            &context(),
        )
        .await
        .unwrap();
    let progress = test.session.snapshot(&PROGRESS_DOC, task_id, &context()).await.unwrap();
    assert_eq!(progress.unwrap(), json(r#"{"lines":["created"]}"#));
    flush().await;
    let publication = test.last_publication();
    assert!(publication.changes.iter().any(|c| matches!(c, CommitChange::Task(task) if task.id == task_id)));
    let documents = document_changes(&publication);
    assert_eq!(documents.len(), 2);
    // Task documents derive their conversation from the task record.
    for document in &documents {
        let DocumentCommitChange::Change { conversation_id: owner, .. } = document else { panic!() };
        assert_eq!(*owner, Some(conversation_id));
    }
    test.session
        .commit(
            move |tx| async move {
                tx.create_conversation(ConversationOwnership::Ownerless).await?;
                tx.doc(&PROGRESS_DOC, task_id).await?.edit(|p| p.lines.push("committed task".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let DocumentCommitChange::Change { conversation_id: owner, .. } = &document_changes(&test.last_publication())[0]
    else {
        panic!()
    };
    assert_eq!(*owner, Some(conversation_id));
}

#[tokio::test]
async fn rejects_task_documents_after_a_terminal_candidate() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let task_id = create_task(&test.session, conversation_id, true).await;
    test.session
        .commit_with(
            move |tx| async move {
                let task = tx.task(task_id).await?.unwrap();
                let progress = tx.doc(&PROGRESS_DOC, task_id).await?;
                tx.set_task(terminal(&task))?;
                let expected = format!("Task {task_id} is terminal");
                assert_eq!(message(tx.doc(&PROGRESS_DOC, task_id).await), expected);
                assert_eq!(message(tx.family_doc(&STEP_DOC, task_id, "late", &()).await), expected);
                assert!(message(tx.set_task(task)).contains("terminal candidate"));
                progress.edit(|p| p.lines.push("final".into()))
            },
            &context(),
            TransactionScope::default(),
        )
        .await
        .unwrap();
    let late = test.session.commit(move |tx| async move { tx.doc(&PROGRESS_DOC, task_id).await.map(drop) }, &context());
    assert_eq!(message(late.await), format!("Task {task_id} is terminal"));
}

#[tokio::test]
async fn retires_task_documents_at_terminal_settlement_including_documents_created_in_the_same_transaction() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let task_id = create_task(&test.session, conversation_id, true).await;
    test.session
        .commit(
            move |tx| async move {
                tx.family_doc(&STEP_DOC, task_id, "committed", &()).await?.edit(|p| p.lines.push("x".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let published = test.publication_count();
    test.session
        .commit_with(
            move |tx| async move {
                let task = tx.task(task_id).await?.unwrap();
                let step = tx.family_doc(&STEP_DOC, task_id, "new", &()).await?;
                step.edit(|p| p.lines.push("created then retired".into()))?;
                tx.set_task(terminal(&task))
            },
            &context(),
            TransactionScope::default(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 5);
    assert_eq!(writes.iter().filter(|w| matches!(w, StorageWrite::Task { .. })).count(), 1);
    let creations: Vec<_> = writes
        .iter()
        .filter_map(|w| match w {
            StorageWrite::DocumentCreate { record, .. } => Some(record.id),
            _ => None,
        })
        .collect();
    let retirements: Vec<_> = writes
        .iter()
        .filter_map(|w| match w {
            StorageWrite::DocumentRetire { id } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(creations.len(), 1);
    assert_eq!(retirements.len(), 3);
    assert!(retirements.contains(&creations[0]));
    flush().await;
    assert_eq!(test.publication_count(), published + 1);
    let publication = test.last_publication();
    assert_eq!(publication.changes.iter().filter(|c| matches!(c, CommitChange::Task(_))).count(), 1);
    let documents = document_changes(&publication);
    assert_eq!(documents.len(), 3);
    for document in &documents {
        let DocumentCommitChange::Retire { conversation_id: owner, .. } = document else { panic!() };
        assert_eq!(*owner, Some(conversation_id));
    }
    let ctx = context();
    assert!(test.session.snapshot(&PROGRESS_DOC, task_id, &ctx).await.unwrap().is_none());
    assert!(test.session.family_snapshot(&STEP_DOC, task_id, "committed", &ctx).await.unwrap().is_none());
    let query = DocumentQuery { scope: DocumentScope::Task { task_id }, at: DocumentPoint::Current, kind: None };
    assert!(test.storage.scan_documents(&query, 10, None, &ctx).await.unwrap().items.is_empty());
    let again = replace_task(&test.session, task_id, terminal).await;
    assert_eq!(message(again), format!("Task {task_id} is already terminal"));
}

#[tokio::test]
async fn validates_document_owners() {
    let test = open_test_session();
    let task = test.session.commit(|tx| async move { tx.doc(&PROGRESS_DOC, TaskId(4242)).await.map(drop) }, &context());
    assert_eq!(message(task.await), "Task 4242 does not exist");
    let notes =
        test.session.commit(|tx| async move { tx.doc(&NOTES_DOC, ConversationId(4242)).await.map(drop) }, &context());
    assert_eq!(message(notes.await), "Conversation 4242 does not exist");
}
