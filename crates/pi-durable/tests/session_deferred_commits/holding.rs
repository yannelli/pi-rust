//! Holding a deferred commit: what may be held, and what is stored at once.

use std::future::Future;

use pi_durable::session::{Session, SessionOptions, Tx, create_session};
use pi_durable::*;
use serde_json::json;

use super::support::*;
use super::*;

#[tokio::test]
async fn adopts_a_commit_that_only_changes_an_existing_latest_conversation_document_in_memory_unstored() {
    let opened = open(None, None).await;
    let (admitted, publications) = (opened.mark(), opened.stored.lock().unwrap().len());
    opened.append("a", CommitOptions::DEFERRED).await.unwrap();
    opened.append("b", CommitOptions::DEFERRED).await.unwrap();
    assert_eq!(opened.mark(), admitted);
    assert_eq!(opened.stored.lock().unwrap().len(), publications);
    assert_eq!(opened.snapshot().await, Some(json(r#"{"text":"ab"}"#)));
    assert_eq!(opened.stored_text().await, Some(json(r#"{"text":""}"#)));
    assert_eq!(opened.held(), 1);
    let observed = opened.observed.lock().unwrap();
    assert!(observed[observed.len() - 2..].iter().all(|publication| publication.is_memory()));
}

async fn stores<F, Fut>(opened: &Opened, change: F) -> bool
where
    F: FnOnce(Tx) -> Fut + Send + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
{
    let admitted = opened.mark();
    opened.session.commit_with_options(change, &context(), CommitOptions::DEFERRED).await.unwrap();
    opened.mark() > admitted
}

async fn set_text<T: pi_durable::documents::DocValue, S: pi_durable::documents::DocScope>(
    tx: &Tx,
    token: &pi_durable::documents::DocToken<T, S>,
    owner: S::Owner,
    text: &str,
) -> DurableResult<()> {
    let draft = tx.doc(token, owner).await?;
    draft.core().update(|state| state.set("text", text))??;
    Ok(())
}

#[tokio::test]
async fn stores_a_deferred_commit_at_once_when_it_writes_anything_else() {
    let opened = open(None, None).await;
    let id = opened.conversation_id;
    // An entry next to the held document's change.
    let entry = move |tx: Tx| async move {
        set_text(&tx, &TEXT_DOC, id, "x").await?;
        tx.append_entry(id, EntryDraft::new("test.note")).await.map(drop)
    };
    assert!(stores(&opened, entry).await);
    // Creation, then other document semantics.
    assert!(stores(&opened, move |tx: Tx| async move { tx.doc(&REWINDABLE_DOC, id).await.map(drop) }).await);
    assert!(stores(&opened, move |tx: Tx| async move { set_text(&tx, &REWINDABLE_DOC, id, "r").await }).await);
    assert!(stores(&opened, move |tx: Tx| async move { tx.doc(&CURRENT_FORK_DOC, id).await.map(drop) }).await);
    assert!(stores(&opened, move |tx: Tx| async move { set_text(&tx, &CURRENT_FORK_DOC, id, "c").await }).await);
    assert!(stores(&opened, |tx: Tx| async move { tx.doc(&SESSION_TEXT_DOC, ()).await.map(drop) }).await);
    assert!(stores(&opened, |tx: Tx| async move { set_text(&tx, &SESSION_TEXT_DOC, (), "s").await }).await);
    // A held document next to an immediate one: the whole commit is stored.
    let both = move |tx: Tx| async move {
        set_text(&tx, &TEXT_DOC, id, "y").await?;
        set_text(&tx, &SESSION_TEXT_DOC, (), "t").await
    };
    assert!(stores(&opened, both).await);
    // Retirement.
    assert!(stores(&opened, move |tx: Tx| async move { tx.retire_doc(&CURRENT_FORK_DOC, id).await }).await);
    assert_eq!(opened.held(), 0);
    // The control: the same change alone is held.
    assert!(!stores(&opened, move |tx: Tx| async move { set_text(&tx, &TEXT_DOC, id, "z").await }).await);
}

#[tokio::test]
async fn stores_a_deferred_commit_that_migrates_its_document_at_once_as_a_base() {
    let opened = open(None, None).await;
    let (mark, id) = (opened.mark(), opened.conversation_id);
    let change = move |tx: Tx| async move { set_text(&tx, &TEXT_DOC_V2, id, "v2").await };
    opened.session.commit_with_options(change, &context(), CommitOptions::DEFERRED).await.unwrap();
    let kinds: Vec<_> = changes_of(&opened.batches_since(mark))
        .iter()
        .map(|(_, content)| content_json(content)["kind"].clone())
        .collect();
    assert_eq!(kinds, [json!("base")]);
    assert_eq!(opened.held(), 0);
}

#[tokio::test]
async fn rejects_a_deferred_commit_on_a_read_only_session_like_any_commit_with_writes() {
    let opened = open(None, None).await;
    let viewer: Session =
        create_session(opened.storage.clone(), SessionOptions { read_only: true, ..SessionOptions::default() });
    let error = append(&viewer, opened.conversation_id, "a", CommitOptions::DEFERRED).await.unwrap_err();
    assert!(matches!(error, DurableError::ReadOnlySession(_)), "{error:?}");
}

#[tokio::test]
async fn rejects_a_within_ms_that_is_not_a_number_of_milliseconds_0_or_more() {
    let opened = open(None, None).await;
    for within in [-1.0, f64::NAN] {
        let error = opened.append("a", CommitOptions::deferred(Some(within))).await.unwrap_err();
        assert!(matches!(error, DurableError::Type(_)), "{error:?}");
        assert_eq!(error.to_string(), "CommitPersistence.withinMs must be a number of milliseconds, 0 or more");
    }
    opened.append("a", IMMEDIATE).await.unwrap();
}
