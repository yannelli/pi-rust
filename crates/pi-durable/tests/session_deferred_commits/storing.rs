//! What stores held changes: a commit that reads or writes the document, the deadline, close, a reload, retirement.

use pi_durable::session::Tx;
use pi_durable::*;
use serde_json::json;

use super::support::*;
use super::*;

#[tokio::test]
async fn stores_them_first_folded_with_its_own_in_a_commit_that_writes_the_document() {
    let opened = open(None, None).await;
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    opened.append("b", CommitOptions::DEFERRED).await.unwrap();
    let mark = opened.mark();
    opened.append("c", IMMEDIATE).await.unwrap();
    let batches = opened.batches_since(mark);
    assert_eq!(batches.len(), 1);
    let contents: Vec<_> = changes_of(&batches).iter().map(|(_, content)| content_json(content)).collect();
    assert_eq!(contents, [json!({ "version": 1, "kind": "delta", "ops": [["a", ["text"], "abc"]] })]);
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"abc"}"#)));
    assert_eq!(opened.held(), 0);
}

#[tokio::test]
async fn stores_them_in_a_commit_that_only_reads_the_document_with_its_other_writes() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    let (mark, id) = (opened.mark(), opened.conversation_id);
    let derive = move |tx: Tx| async move {
        let text = tx.doc(&TEXT_DOC, id).await?.value()?["text"].clone();
        let draft = EntryDraft { data: Some(text), ..EntryDraft::new("test.derived") };
        tx.append_entry(id, draft).await.map(drop)
    };
    opened.session.commit(derive, &context()).await.unwrap();
    let batches = opened.batches_since(mark);
    assert_eq!(batches.len(), 1);
    let mut kinds: Vec<_> = batches[0].iter().map(content_kind_of).collect();
    kinds.sort();
    assert_eq!(kinds, ["document.change", "entry"]);
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held"}"#)));
}

fn content_kind_of(write: &StorageWrite) -> &'static str {
    match write {
        StorageWrite::DocumentChange { .. } => "document.change",
        StorageWrite::DocumentRetire { .. } => "document.retire",
        StorageWrite::Entry { .. } => "entry",
        _ => "other",
    }
}

#[tokio::test]
async fn leaves_them_held_through_commits_that_neither_read_nor_write_the_document_or_write_nothing() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    let (mark, id) = (opened.mark(), opened.conversation_id);
    let unrelated = move |tx: Tx| async move { tx.append_entry(id, EntryDraft::new("test.unrelated")).await.map(drop) };
    opened.session.commit(unrelated, &context()).await.unwrap();
    let read = move |tx: Tx| async move { tx.doc(&TEXT_DOC, id).await.map(drop) };
    opened.session.commit(read, &context()).await.unwrap();
    assert!(changes_of(&opened.batches_since(mark)).is_empty());
    assert_eq!(opened.held(), 1);
}

#[tokio::test]
async fn keeps_them_when_a_commit_that_reads_the_document_fails_before_storage() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    let id = opened.conversation_id;
    let failing = move |tx: Tx| async move {
        tx.doc(&TEXT_DOC, id).await?.core().update(|state| state.set("text", "held!"))??;
        tx.append_entry(id, EntryDraft::new("test.note")).await?;
        Err::<(), _>(DurableError::error("callback failed"))
    };
    assert_eq!(message(opened.session.commit(failing, &context()).await), "callback failed");
    // Its checkpoint predicate fails: settlement fails after every other step.
    let exploding = move |tx: Tx| async move {
        tx.doc(&TEXT_DOC, id).await?.core().update(|state| state.set("explode", true))??;
        Ok(())
    };
    assert_eq!(message(opened.session.commit(exploding, &context()).await), "checkpoint failed");
    assert_eq!(opened.held(), 1);
    opened.fire().await;
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held"}"#)));
}

#[tokio::test]
async fn stores_every_held_document_in_one_transaction_when_the_earliest_deadline_comes() {
    let opened = open(None, None).await;
    let other = create_conversation(&opened.session).await;
    let create = move |tx: Tx| async move { tx.doc(&TEXT_DOC, other).await.map(drop) };
    opened.session.commit(create, &context()).await.unwrap();
    let other_id = change_record(&document_changes(&opened.stored.lock().unwrap().last().cloned().unwrap())[0]).id;
    opened.append("one", CommitOptions::DEFERRED).await.unwrap();
    append(&opened.session, other, "two", CommitOptions::DEFERRED).await.unwrap();
    assert_eq!(opened.armed(), [1_000.0]);
    let (publications, mark) = (opened.stored.lock().unwrap().len(), opened.mark());
    opened.fire().await;
    let batches = opened.batches_since(mark);
    assert_eq!(batches.len(), 1);
    assert_eq!(changes_of(&batches).len(), 2);
    assert_eq!(opened.stored.lock().unwrap().len(), publications + 1);
    assert_eq!(opened.held(), 0);
    assert!(opened.armed().is_empty());
    assert_eq!(stored_value(&opened.storage, other_id).await, Some(json(r#"{"text":"two"}"#)));
}

#[tokio::test]
async fn arms_the_earliest_deadline_of_the_held_commits_each_with_its_within_ms_or_the_default() {
    let opened = open(None, Some(2_000.0)).await;
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    assert_eq!(opened.armed(), [2_000.0]);
    *opened.clock.lock().unwrap() += 500.0;
    // Earlier: the timer moves up.
    opened.append("b", CommitOptions::deferred(Some(100.0))).await.unwrap();
    assert_eq!(opened.armed(), [100.0]);
    // Later: the deadline stays.
    opened.append("c", CommitOptions::deferred(Some(60_000.0))).await.unwrap();
    assert_eq!(opened.armed(), [100.0]);
}

#[tokio::test]
async fn arms_no_timer_for_an_infinite_window_and_moves_the_timer_on_when_a_commit_stores_the_held_document() {
    let opened = open(None, None).await;
    opened.append("a", CommitOptions::deferred(Some(f64::INFINITY))).await.unwrap();
    assert!(opened.armed().is_empty());
    assert_eq!(opened.held(), 1);
    opened.append("b", CommitOptions::DEFERRED).await.unwrap();
    assert_eq!(opened.armed(), [1_000.0]);
    opened.append("c", IMMEDIATE).await.unwrap();
    assert!(opened.armed().is_empty());
    assert_eq!(opened.held(), 0);
}

#[tokio::test]
async fn stores_them_at_close_after_the_commits_admitted_before_it_and_closes_storage_when_that_store_fails() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    opened.session.close(&context()).await.unwrap();
    opened.storage.reopen();
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held"}"#)));

    let failing = open(None, None).await;
    failing.append("lost", CommitOptions::DEFERRED).await.unwrap();
    failing.storage.fail_next_commit(DurableError::storage("disk gone"));
    assert_eq!(message(failing.session.close(&context()).await), "disk gone");
    assert!(
        matches!(failing.session.closed().await, session::SessionEnd::Failed(error) if error.to_string() == "disk gone")
    );
    // Closed: the backend refuses calls until reopened.
    let refused = failing.storage.document(failing.record_id, DocumentPoint::Current, &context()).await;
    assert!(message(refused).contains("closed"));
    failing.storage.reopen();
    assert_eq!(failing.stored_text().await, Some(json(r#"{"text":""}"#)));
}

#[tokio::test]
async fn stores_nothing_once_the_session_failed() {
    let opened = open(None, None).await;
    opened.append("lost", CommitOptions::DEFERRED).await.unwrap();
    let mark = opened.mark();
    opened.session.fail(DurableError::error("crash"), None);
    opened.session.closed().await;
    assert!(opened.batches_since(mark).is_empty());
    opened.storage.reopen();
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":""}"#)));
}

#[tokio::test]
async fn stores_them_before_unload_documents_drops_the_trackers() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    opened.session.unload_documents().await.unwrap();
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held"}"#)));
    let sizes = opened.session.document_cache_sizes();
    assert_eq!((sizes.cached, sizes.held), (0, 0));
    assert_eq!(opened.snapshot().await, Some(json(r#"{"text":"held"}"#)));
}

#[tokio::test]
async fn stores_them_before_a_token_of_another_definition_version_reloads_the_document() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    let (mark, id) = (opened.mark(), opened.conversation_id);
    let migrated = opened.session.snapshot(&TEXT_DOC_V2, id, &context()).await.unwrap();
    assert_eq!(migrated, Some(json(r#"{"text":"held","migrated":true}"#)));
    let kinds: Vec<_> = changes_of(&opened.batches_since(mark))
        .iter()
        .map(|(_, content)| content_json(content)["kind"].clone())
        .collect();
    assert_eq!(kinds, [json!("delta")]);
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held"}"#)));
    // The same inside a commit, which then migrates with a base.
    opened
        .session
        .commit(move |tx: Tx| async move { tx.doc(&TEXT_DOC, id).await.map(drop) }, &context())
        .await
        .unwrap();
    opened.append("+", CommitOptions::DEFERRED).await.unwrap();
    let change = move |tx: Tx| async move {
        let draft = tx.doc(&TEXT_DOC_V2, id).await?;
        draft.update(|state| {
            let next = format!("{}v2", state["text"].as_str().unwrap_or_default());
            state.set("text", next)
        })??;
        Ok(())
    };
    opened.session.commit(change, &context()).await.unwrap();
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":"held+v2","migrated":true}"#)));
}

#[tokio::test]
async fn stores_them_before_the_document_s_retirement_in_the_retiring_commit() {
    let opened = open(None, None).await;
    opened.append("held", CommitOptions::DEFERRED).await.unwrap();
    let (mark, id) = (opened.mark(), opened.conversation_id);
    opened.session.commit(move |tx: Tx| async move { tx.retire_doc(&TEXT_DOC, id).await }, &context()).await.unwrap();
    let batches = opened.batches_since(mark);
    assert_eq!(batches[0].iter().map(content_kind_of).collect::<Vec<_>>(), ["document.change", "document.retire"]);
    let content = content_json(&changes_of(&batches)[0].1);
    assert_eq!(content, json!({ "version": 1, "kind": "delta", "ops": [["a", ["text"], "held"]] }));
    assert_eq!(opened.held(), 0);
    assert_eq!(opened.snapshot().await, None);
}
