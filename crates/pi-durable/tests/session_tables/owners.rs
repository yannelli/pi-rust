//! Conversation ownership validation and ownership of table JSON.

use std::sync::{Arc, Mutex};

use pi_durable::session::TransactionScope;
use pi_durable::tasks::TaskOptions;
use pi_durable::*;

use super::support::*;
use super::*;

#[tokio::test]
async fn creates_a_conversation_with_an_explicitly_staged_task_owner() {
    let test = open_test_session();
    let parent_id = create_conversation(&test.session).await;
    let (supervisor_id, child) = test
        .session
        .commit(
            move |tx| async move {
                let options = TaskOptions { background: true, ..TaskOptions::conversation(parent_id) };
                let supervisor_id = tx.create_task(work(), json(r#"{"path":"background"}"#), options).await?;
                let child = tx.create_conversation(ConversationOwnership::Task { task_id: supervisor_id }).await?;
                Ok((supervisor_id, child))
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(child.owner, Some(ConversationOwner { conversation_id: parent_id, task_id: supervisor_id }));
    assert_eq!(test.storage.conversation(child.id, &context()).await.unwrap(), Some(child));
    let moved = test
        .session
        .commit_with(
            move |tx| async move {
                let supervisor = tx.task(supervisor_id).await?.unwrap();
                tx.set_task(TaskRecord { conversation_id: child.id, ..supervisor })
            },
            &context(),
            TransactionScope::default(),
        )
        .await;
    assert_eq!(message(moved), format!("Task {supervisor_id} cannot change conversations"));
    let stored = test.storage.task(supervisor_id, &context()).await.unwrap().unwrap();
    assert_eq!(stored.conversation_id, parent_id);
}

#[tokio::test]
async fn rejects_missing_terminal_and_abort_marked_conversation_owners_atomically() {
    let test = open_test_session();
    let parent_id = create_conversation(&test.session).await;
    let ctx = context();
    let scope = TransactionScope::default();
    let moved_id: Arc<Mutex<Option<TaskId>>> = Arc::default();
    let slot = moved_id.clone();
    let moved = test
        .session
        .commit_with(
            move |tx| async move {
                let options = TaskOptions::conversation(parent_id);
                let task_id = tx.create_task(work(), json(r#"{"path":"move"}"#), options).await?;
                *slot.lock().unwrap() = Some(task_id);
                tx.set_task(pending(task_id, ConversationId(998), "move", false))
            },
            &ctx,
            scope,
        )
        .await;
    assert!(message(moved).contains("cannot change conversations"));
    let moved_id = moved_id.lock().unwrap().expect("a staged task ID");
    assert!(test.storage.task(moved_id, &ctx).await.unwrap().is_none());

    let missing = test
        .session
        .commit(
            |tx| async move { tx.create_conversation(ConversationOwnership::Task { task_id: TaskId(999) }).await },
            &ctx,
        )
        .await;
    assert_eq!(message(missing), "Conversation owner task 999 does not exist");

    let rejected: Arc<Mutex<Option<ConversationId>>> = Arc::default();
    let slot = rejected.clone();
    let aborting = test
        .session
        .commit_with(
            move |tx| async move {
                let options = TaskOptions::conversation(parent_id);
                let supervisor = tx.create_task(work(), json(r#"{"path":"aborting"}"#), options).await?;
                let child = tx.create_conversation(ConversationOwnership::Task { task_id: supervisor }).await?;
                *slot.lock().unwrap() = Some(child.id);
                tx.set_task(pending(supervisor, parent_id, "aborting", true))
            },
            &ctx,
            scope,
        )
        .await;
    assert!(message(aborting).contains("is abort-marked"));
    let rejected_id = rejected.lock().unwrap().expect("a rejected child ID");
    assert!(test.storage.conversation(rejected_id, &ctx).await.unwrap().is_none());

    let finishing = test
        .session
        .commit_with(
            move |tx| async move {
                let options = TaskOptions::conversation(parent_id);
                let supervisor = tx.create_task(work(), json(r#"{"path":"terminal"}"#), options).await?;
                tx.create_conversation(ConversationOwnership::Task { task_id: supervisor }).await?;
                tx.set_task(terminal(&pending(supervisor, parent_id, "terminal", false)))
            },
            &ctx,
            scope,
        )
        .await;
    assert!(message(finishing).contains("is terminal"));

    let terminal_owner = create_task(&test.session, parent_id, false).await;
    replace_task(&test.session, terminal_owner, terminal).await.unwrap();
    let late =
        test.session
            .commit(
                move |tx| async move {
                    tx.create_conversation(ConversationOwnership::Task { task_id: terminal_owner }).await
                },
                &ctx,
            )
            .await;
    assert!(message(late).contains("is terminal"));
}

#[tokio::test]
async fn takes_ownership_of_table_json() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let ctx = context();
    let mut payload = json(r#"{"nested":{"value":1}}"#);
    let draft = EntryDraft { data: Some(payload.clone()), ..EntryDraft::new("data") };
    let entry = test
        .session
        .commit(move |tx| async move { tx.append_entry(conversation_id, draft).await }, &ctx)
        .await
        .unwrap();
    payload.get_mut("nested").unwrap().set("value", 2).unwrap();
    let stored = test.storage.entry(entry.id, &ctx).await.unwrap().unwrap();
    assert_eq!(stored.entry.data, Some(json(r#"{"nested":{"value":1}}"#)));
    let omitted = test
        .session
        .commit(move |tx| async move { tx.append_entry(conversation_id, EntryDraft::new("omitted")).await }, &ctx)
        .await
        .unwrap();
    assert!(omitted.data.is_none());
    let stored = test.storage.entry(omitted.id, &ctx).await.unwrap().unwrap();
    let encoded = serde_json::to_string(&stored.entry).unwrap();
    assert!(!encoded.contains("\"data\""));
    // Upstream also rejects `data: NaN`. `Json` cannot hold a non-finite number, so that case cannot arise here.
}
