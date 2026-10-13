//! Port of upstream `test/session-checkpoints-migrations.test.ts`.

#[path = "../support/mod.rs"]
mod support;

mod cache;
mod checkpoints;
mod history;
mod migrations;

use pi_chord::Json;
use pi_durable::documents::*;
use pi_durable::session::Session;
use pi_durable::*;
use support::*;

pub type SessionDoc = DocToken<Json, SessionScope>;

/// A Session-scoped JSON document with the given first value.
pub fn session_doc(kind: &str, version: u32, initial: &str) -> DocSpec<Json, SessionScope> {
    let initial = json(initial);
    DocSpec::new(kind, version, SessionScope, move || initial.clone())
}

/// Access a document in its own commit, creating or migrating it.
pub async fn touch<S: DocScope>(session: &Session, token: &DocToken<Json, S>, owner: S::Owner) -> DurableResult<()> {
    let token = token.clone();
    session.commit(move |tx| async move { tx.doc(&token, owner).await.map(drop) }, &context()).await
}

/// Set one field of a document in its own commit.
pub async fn set_field<S: DocScope>(
    session: &Session,
    token: &DocToken<Json, S>,
    owner: S::Owner,
    key: &'static str,
    value: Json,
) -> DurableResult<()> {
    let token = token.clone();
    session
        .commit(
            move |tx| async move { tx.doc(&token, owner).await?.update(|doc| doc.set(key, value))?.map_err(Into::into) },
            &context(),
        )
        .await
}

/// Add one to the `count` field.
pub async fn increment(session: &Session, token: &SessionDoc) {
    let token = token.clone();
    session
        .commit(
            move |tx| async move {
                tx.doc(&token, ())
                    .await?
                    .update(|doc| {
                        let count = doc["count"].as_i64().unwrap();
                        doc.set("count", count + 1)
                    })?
                    .map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
}

pub async fn snapshot(session: &Session, token: &SessionDoc) -> DurableResult<Option<Json>> {
    session.snapshot(token, (), &context()).await
}

/// `"base"` or `"delta"` of every document change write, across commits.
pub fn change_kinds(commits: &[Vec<StorageWrite>]) -> Vec<&'static str> {
    let changes = commits.iter().flatten().filter(|w| matches!(w, StorageWrite::DocumentChange { .. }));
    changes.filter_map(content_kind).collect()
}

/// The content of the first write of the newest commit.
pub fn first_content(test: &TestSession) -> Option<DocumentContent> {
    match test.storage.last_commit().into_iter().next()? {
        StorageWrite::DocumentChange { content, .. } | StorageWrite::DocumentCreate { content, .. } => Some(content),
        _ => None,
    }
}
