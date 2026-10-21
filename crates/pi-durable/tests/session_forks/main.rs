//! Port of upstream `test/session-forks.test.ts`.

#[path = "../support/mod.rs"]
mod support;

mod copies;
mod rejections;

use std::future::Future;

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::{Session, Tx};
use pi_durable::*;
use support::*;

pub fn rewindable(kind: &str, initial: &str) -> DocToken<Json, RewindableConversation> {
    let initial = json(initial);
    define_doc(DocSpec::new(
        kind,
        1,
        RewindableConversation(DocumentFork::AsOf),
        move || initial.clone(),
        |_, _, _| Ok(false),
    ))
    .unwrap()
}

pub fn latest(kind: &str, fork: LatestFork, initial: &str) -> DocToken<Json, LatestConversation> {
    let initial = json(initial);
    define_doc(DocSpec::new(kind, 1, LatestConversation(fork), move || initial.clone(), |_, _, _| Ok(false))).unwrap()
}

/// Set `value` of a conversation document inside a transaction.
pub async fn set_value<S: DocScope>(
    tx: &Tx,
    token: &DocToken<Json, S>,
    owner: S::Owner,
    value: impl Into<Json>,
) -> DurableResult<()> {
    let value = value.into();
    tx.doc(token, owner).await?.update(|d| d.set("value", value))?.map_err(Into::into)
}

/// Append an entry of `kind`, then run `edit` in the same commit; returns the entry ID.
pub async fn entry_with<F, Fut>(
    session: &Session,
    conversation_id: ConversationId,
    kind: &'static str,
    edit: F,
) -> EntryId
where
    F: FnOnce(Tx) -> Fut + Send + 'static,
    Fut: Future<Output = DurableResult<()>> + Send + 'static,
{
    session
        .commit(
            move |tx| async move {
                let entry = tx.append_entry(conversation_id, EntryDraft::new(kind)).await?;
                edit(tx).await?;
                Ok(entry.id)
            },
            &context(),
        )
        .await
        .unwrap()
}

pub fn fork(
    session: &Session,
    parent: ConversationId,
    at: EntryId,
) -> impl Future<Output = DurableResult<ConversationRecord>> {
    session.commit(
        move |tx| async move { tx.fork_conversation(parent, at, ConversationOwnership::Ownerless).await },
        &context(),
    )
}

pub async fn snapshot<S: DocScope>(session: &Session, token: &DocToken<Json, S>, owner: S::Owner) -> Option<Json> {
    session.snapshot(token, owner, &context()).await.unwrap()
}

pub fn copies(writes: &[StorageWrite]) -> Vec<(DocumentCreate, DocumentCopySource)> {
    let copies = writes.iter().filter_map(|write| match write {
        StorageWrite::DocumentCopy { record, source } => Some((record.clone(), *source)),
        _ => None,
    });
    copies.collect()
}

pub fn count<F: Fn(&StorageWrite) -> bool>(writes: &[StorageWrite], predicate: F) -> usize {
    writes.iter().filter(|write| predicate(write)).count()
}

pub async fn find_record(test: &TestSession, kind: &str, conversation_id: ConversationId) -> Option<DocumentRecord> {
    let scope = DocumentScope::Conversation { conversation_id };
    let address = DocumentAddress { kind: kind.into(), scope, key: None };
    test.storage.find_document(&address, DocumentPoint::Current, &context()).await.unwrap()
}
