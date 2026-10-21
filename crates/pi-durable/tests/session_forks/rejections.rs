//! Typed access inside the fork transaction, rejected forks, and rollback of copies.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pi_durable::session::SessionEnd;

use super::support::*;
use super::*;

#[tokio::test]
async fn coalesces_a_typed_migration_and_override_into_the_copied_creation_base() {
    let v1 = rewindable("fork.override", r#"{"count":2}"#);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let counter = checkpoints.clone();
    let initial = json(r#"{"count":0,"migrated":false}"#);
    let spec = DocSpec::new(
        "fork.override",
        2,
        RewindableConversation(DocumentFork::AsOf),
        move || initial.clone(),
        move |_, _, _| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(false)
        },
    )
    .migrate(|value, _| Ok(json(&format!(r#"{{"count":{},"migrated":true}}"#, value["count"]))));
    let v2 = define_doc(spec).unwrap();
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = v1.clone();
    let fork_at =
        entry_with(&test.session, parent, "point", move |tx| async move { tx.doc(&token, parent).await.map(drop) })
            .await;
    let reads = test.storage.document_reads();
    let token = v2.clone();
    let child = test
        .session
        .commit(
            move |tx| async move {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless).await?;
                tx.doc(&token, created.id).await?.update(|d| d.set("count", 9))??;
                Ok(created)
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert_eq!(test.storage.document_reads(), reads + 1);
    let writes = test.storage.last_commit();
    let creates: Vec<_> = writes.iter().filter(|w| matches!(w, StorageWrite::DocumentCreate { .. })).collect();
    assert_eq!(creates.len(), 1);
    let StorageWrite::DocumentCreate { content: DocumentContent::Base { version: 2, value }, .. } = creates[0] else {
        panic!("expected a version 2 base")
    };
    assert_eq!(*value, json(r#"{"count":9,"migrated":true}"#));
    assert_eq!(checkpoints.load(Ordering::SeqCst), 0);
    let published = document_changes(&test.last_publication());
    assert!(matches!(published[..], [DocumentCommitChange::Document { version: Some(2), .. }]));
    assert_eq!(snapshot(&test.session, &v2, child.id).await.unwrap(), json(r#"{"count":9,"migrated":true}"#));
    let record = find_record(&test, "fork.override", parent).await.unwrap();
    let stored = test.storage.document(record.id, DocumentPoint::Current, &context()).await.unwrap().unwrap();
    assert_eq!(stored.version, 1);
}

#[tokio::test]
async fn rejects_invisible_fork_points_before_admission_and_remains_usable() {
    let test = open_test_session();
    let root = create_conversation(&test.session).await;
    let (visible, hidden) = test
        .session
        .commit(
            move |tx| async move {
                let visible = tx.append_entry(root, EntryDraft::new("visible")).await?;
                let hidden = tx.append_entry(root, EntryDraft::new("hidden")).await?;
                Ok((visible.id, hidden.id))
            },
            &context(),
        )
        .await
        .unwrap();
    let parent = fork(&test.session, root, visible).await.unwrap();
    flush().await;
    let (commits, published) = (test.storage.commit_count(), test.publication_count());
    let rejected = fork(&test.session, parent.id, hidden).await;
    assert!(message(rejected).contains(&format!("Entry {hidden} is not visible")));
    flush().await;
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.publication_count(), published);
    let independent = test
        .session
        .commit(|tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await }, &context())
        .await
        .unwrap();
    assert_eq!(test.storage.conversation(independent.id, &context()).await.unwrap(), Some(independent));
}

#[tokio::test]
async fn rejects_duplicate_as_of_and_current_selections_for_one_child_address_before_admission() {
    let as_of = rewindable("fork.duplicate-policy", r#"{"value":"as-of"}"#);
    let current = latest("fork.duplicate-policy", LatestFork::Current, r#"{"value":"current"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = as_of.clone();
    let old_at =
        entry_with(&test.session, parent, "old", move |tx| async move { tx.doc(&token, parent).await.map(drop) }).await;
    let token = as_of.clone();
    test.session.commit(move |tx| async move { tx.retire_doc(&token, parent).await }, &context()).await.unwrap();
    let token = current.clone();
    test.session.commit(move |tx| async move { tx.doc(&token, parent).await.map(drop) }, &context()).await.unwrap();
    let commits = test.storage.commit_count();
    let rejected = fork(&test.session, parent, old_at).await;
    assert!(message(rejected).contains("Fork selects multiple source documents"));
    assert_eq!(test.storage.commit_count(), commits);
    let independent = test
        .session
        .commit(|tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await }, &context())
        .await
        .unwrap();
    assert_eq!(test.storage.conversation(independent.id, &context()).await.unwrap(), Some(independent));
}

#[tokio::test]
async fn rejects_current_and_as_of_source_writes_in_the_fork_transaction() {
    let current = latest("fork.same-transaction-current", LatestFork::Current, r#"{"value":"committed"}"#);
    let as_of = rewindable("fork.same-transaction-as-of", r#"{"value":"committed"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let tokens = (current.clone(), as_of.clone());
    let fork_at = entry_with(&test.session, parent, "point", move |tx| async move {
        tx.doc(&tokens.0, parent).await?;
        tx.doc(&tokens.1, parent).await.map(drop)
    })
    .await;
    let commits = test.storage.commit_count();
    let ownerless = ConversationOwnership::Ownerless;
    let token = current.clone();
    let before = test.session.commit(
        move |tx| async move {
            set_value(&tx, &token, parent, "before-fork").await?;
            tx.fork_conversation(parent, fork_at, ownerless).await.map(drop)
        },
        &context(),
    );
    assert!(message(before.await).contains("Cannot change fork source document"));
    let token = current.clone();
    let after = test.session.commit(
        move |tx| async move {
            tx.fork_conversation(parent, fork_at, ownerless).await?;
            set_value(&tx, &token, parent, "after-fork").await
        },
        &context(),
    );
    assert!(message(after.await).contains("Cannot change fork source document"));
    let token = as_of.clone();
    let as_of_write = test.session.commit(
        move |tx| async move {
            set_value(&tx, &token, parent, "as-of-write").await?;
            tx.fork_conversation(parent, fork_at, ownerless).await.map(drop)
        },
        &context(),
    );
    assert!(message(as_of_write.await).contains("Cannot change fork source document"));
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(snapshot(&test.session, &current, parent).await.unwrap(), json(r#"{"value":"committed"}"#));
    let child = fork(&test.session, parent, fork_at).await.unwrap();
    assert_eq!(snapshot(&test.session, &current, child.id).await.unwrap(), json(r#"{"value":"committed"}"#));
}

#[tokio::test]
async fn rolls_every_copied_base_back_when_later_pre_admission_assembly_fails() {
    let copied = rewindable("fork.rollback.copied", r#"{"value":"copied"}"#);
    let fail_checkpoint = Arc::new(AtomicBool::new(true));
    let fail = fail_checkpoint.clone();
    let spec = DocSpec::new(
        "fork.rollback.failure",
        1,
        SessionScope,
        || json(r#"{"count":0}"#),
        move |_, _, _| match fail.load(Ordering::SeqCst) {
            true => Err(DurableError::error("checkpoint failed")),
            false => Ok(false),
        },
    );
    let failure = define_doc(spec).unwrap();
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let tokens = (copied.clone(), failure.clone());
    let fork_at = entry_with(&test.session, parent, "point", move |tx| async move {
        tx.doc(&tokens.0, parent).await?;
        tx.doc(&tokens.1, ()).await.map(drop)
    })
    .await;
    flush().await;
    let (commits, published) = (test.storage.commit_count(), test.publication_count());
    let token = failure.clone();
    let child: Arc<std::sync::Mutex<Option<ConversationId>>> = Arc::default();
    let slot = child.clone();
    let failed = test
        .session
        .commit(
            move |tx| async move {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless).await?;
                *slot.lock().unwrap() = Some(created.id);
                tx.doc(&token, ()).await?.update(|d| d.set("count", 1))?.map_err(Into::into)
            },
            &context(),
        )
        .await;
    assert_eq!(message(failed), "checkpoint failed");
    flush().await;
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.publication_count(), published);
    let child_id = child.lock().unwrap().unwrap();
    assert!(test.storage.conversation(child_id, &context()).await.unwrap().is_none());
    fail_checkpoint.store(false, Ordering::SeqCst);
    let token = failure.clone();
    test.session
        .commit(
            move |tx| async move { tx.doc(&token, ()).await?.update(|d| d.set("count", 2))?.map_err(Into::into) },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot(&test.session, &failure, ()).await.unwrap(), json(r#"{"count":2}"#));
    assert_eq!(snapshot(&test.session, &copied, parent).await.unwrap(), json(r#"{"value":"copied"}"#));
}

#[tokio::test]
async fn fails_the_session_on_a_rejected_fork_commit_which_leaves_no_effect() {
    let doc = rewindable("fork.storage-rejected", r#"{"value":"source"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = doc.clone();
    let fork_at =
        entry_with(&test.session, parent, "point", move |tx| async move { tx.doc(&token, parent).await.map(drop) })
            .await;
    test.storage.fail_next_commit(DurableError::storage("copy rejected"));
    let child: Arc<std::sync::Mutex<Option<ConversationId>>> = Arc::default();
    let slot = child.clone();
    let rejected = test
        .session
        .commit(
            move |tx| async move {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless).await?;
                *slot.lock().unwrap() = Some(created.id);
                Ok(())
            },
            &context(),
        )
        .await;
    assert_eq!(message(rejected), "copy rejected");
    let later = test
        .session
        .commit(|tx| async move { tx.create_conversation(ConversationOwnership::Ownerless).await }, &context())
        .await
        .unwrap_err();
    assert_eq!(later.name(), "SessionFailed");
    assert_eq!(later.session_failure_cause().unwrap().to_string(), "copy rejected");
    // The failed Session closed its Storage; reopened, it holds nothing of the batch.
    let end = test.session.closed().await;
    let SessionEnd::Failed(error) = end else { panic!("expected a failed Session") };
    assert_eq!(error.to_string(), "copy rejected");
    let child_id = child.lock().unwrap().unwrap();
    assert!(test.storage.reopen().conversation(child_id, &context()).await.unwrap().is_none());
}

#[tokio::test]
async fn retires_a_copied_document_and_can_recreate_the_address_in_the_fork_transaction() {
    let doc = rewindable("fork.retire-recreate", r#"{"value":"fresh"}"#);
    let test = open_test_session();
    let parent = create_conversation(&test.session).await;
    let token = doc.clone();
    let fork_at =
        entry_with(
            &test.session,
            parent,
            "point",
            move |tx| async move { set_value(&tx, &token, parent, "copied").await },
        )
        .await;
    let token = doc.clone();
    let child = test
        .session
        .commit(
            move |tx| async move {
                let created = tx.fork_conversation(parent, fork_at, ConversationOwnership::Ownerless).await?;
                tx.retire_doc(&token, created.id).await?;
                set_value(&tx, &token, created.id, "replacement").await?;
                Ok(created)
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(count(&writes, |w| matches!(w, StorageWrite::DocumentCopy { .. })), 1);
    assert_eq!(count(&writes, |w| matches!(w, StorageWrite::DocumentCreate { .. })), 1);
    assert_eq!(count(&writes, |w| matches!(w, StorageWrite::DocumentRetire { .. })), 1);
    assert_eq!(snapshot(&test.session, &doc, child.id).await.unwrap(), json(r#"{"value":"replacement"}"#));
}
