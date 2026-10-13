//! Session historical document snapshots.

use std::sync::{Arc, Mutex};

use pi_durable::session::Session;

use super::support::*;
use super::*;

type Rewindable = DocToken<Json, RewindableConversation>;

fn rewindable(kind: &str, version: u32, initial: &str) -> DocSpec<Json, RewindableConversation> {
    let initial = json(initial);
    DocSpec::new(kind, version, RewindableConversation(DocumentFork::AsOf), move || initial.clone())
}

/// Append an entry of `kind`, then run `edit` in the same commit; returns the entry ID.
async fn entry_with<F, Fut>(session: &Session, conversation_id: ConversationId, kind: &'static str, edit: F) -> EntryId
where
    F: FnOnce(session::Tx) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = DurableResult<()>> + Send + 'static,
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

async fn as_of(
    session: &Session,
    token: &Rewindable,
    conversation_id: ConversationId,
    at: EntryId,
) -> DurableResult<Option<Json>> {
    session.snapshot_as_of(token, conversation_id, at, &context()).await
}

#[tokio::test]
async fn migrates_current_and_historical_rewindable_values_independently_and_follows_fork_ancestry() {
    let v1 = define_doc(rewindable("history.migration", 1, r#"{"count":0}"#)).unwrap();
    let family_spec =
        DocFamilySpec::new("history.family", 1, RewindableConversation(DocumentFork::AsOf), |seed: String| {
            json(&format!(r#"{{"seed":"{seed}","count":0}}"#))
        });
    let family = define_doc_family(family_spec).unwrap();
    let migrations: Arc<Mutex<Vec<u32>>> = Arc::default();
    let sink = migrations.clone();
    let v3 =
        define_doc(rewindable("history.migration", 3, r#"{"count":0,"version":3}"#).migrate(move |value, from| {
            sink.lock().unwrap().push(from);
            Ok(json(&format!(r#"{{"count":{},"version":3}}"#, value["count"])))
        }))
        .unwrap();
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let (token, member) = (v1.clone(), family.clone());
    let first = entry_with(&test.session, conversation_id, "first", move |tx| async move {
        tx.doc(&token, conversation_id).await?.update(|d| d.set("count", 1))??;
        let draft = tx.family_doc(&member, conversation_id, "member", &"seed".to_owned()).await?;
        draft.update(|d| d.set("count", 1))?.map_err(Into::into)
    })
    .await;
    let token = v1.clone();
    let second = entry_with(&test.session, conversation_id, "second", move |tx| async move {
        tx.doc(&token, conversation_id).await?.update(|d| d.set("count", 2))?.map_err(Into::into)
    })
    .await;
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    let current = test.session.snapshot(&v3, conversation_id, &context()).await.unwrap().unwrap();
    assert_eq!(current, json(r#"{"count":2,"version":3}"#));
    assert_eq!(*migrations.lock().unwrap(), [1]);
    assert_eq!(test.storage.commit_count(), commits);
    let token = v3.clone();
    let third = entry_with(&test.session, conversation_id, "third", move |tx| async move {
        tx.doc(&token, conversation_id).await.map(drop)
    })
    .await;
    assert!(
        test.storage.last_commit().iter().any(|w| matches!(
            w,
            StorageWrite::DocumentChange { content: DocumentContent::Base { version: 3, .. }, .. }
        ))
    );
    assert_eq!(
        as_of(&test.session, &v3, conversation_id, first).await.unwrap().unwrap(),
        json(r#"{"count":1,"version":3}"#)
    );
    assert_eq!(
        as_of(&test.session, &v3, conversation_id, second).await.unwrap().unwrap(),
        json(r#"{"count":2,"version":3}"#)
    );
    assert_eq!(
        as_of(&test.session, &v3, conversation_id, third).await.unwrap().unwrap(),
        json(r#"{"count":2,"version":3}"#)
    );
    assert_eq!(*migrations.lock().unwrap(), [1, 1, 1]);
    let ctx = context();
    let member = test.session.family_snapshot_as_of(&family, conversation_id, "member", first, &ctx).await.unwrap();
    assert_eq!(member.unwrap(), json(r#"{"seed":"seed","count":1}"#));
    let child_id = test
        .session
        .commit(
            move |tx| async move {
                Ok(tx.fork_conversation(conversation_id, second, ConversationOwnership::Ownerless).await?.id)
            },
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(as_of(&test.session, &v3, child_id, first).await.unwrap().unwrap(), json(r#"{"count":1,"version":3}"#));
    assert_eq!(as_of(&test.session, &v3, child_id, second).await.unwrap().unwrap(), json(r#"{"count":2,"version":3}"#));
    let member = test.session.family_snapshot_as_of(&family, child_id, "member", first, &ctx).await.unwrap();
    assert_eq!(member.unwrap(), json(r#"{"seed":"seed","count":1}"#));
    let hidden = as_of(&test.session, &v3, child_id, third).await;
    assert!(message(hidden).contains(&format!("Entry {third} is not visible")));
}

#[tokio::test]
async fn selects_the_incarnation_alive_at_the_entry_commit_across_retirement_and_recreation() {
    let doc = define_doc(rewindable("history.incarnation", 1, r#"{"value":"initial"}"#)).unwrap();
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let before = entry_with(&test.session, conversation_id, "before", |_| async { Ok(()) }).await;
    let token = doc.clone();
    let created = entry_with(&test.session, conversation_id, "create", move |tx| async move {
        tx.doc(&token, conversation_id).await?.update(|d| d.set("value", "old"))?.map_err(Into::into)
    })
    .await;
    let token = doc.clone();
    let retired = entry_with(&test.session, conversation_id, "retire", move |tx| async move {
        tx.retire_doc(&token, conversation_id).await
    })
    .await;
    let token = doc.clone();
    let recreated = entry_with(&test.session, conversation_id, "recreate", move |tx| async move {
        tx.doc(&token, conversation_id).await?.update(|d| d.set("value", "new"))?.map_err(Into::into)
    })
    .await;
    assert!(as_of(&test.session, &doc, conversation_id, before).await.unwrap().is_none());
    assert_eq!(
        as_of(&test.session, &doc, conversation_id, created).await.unwrap().unwrap(),
        json(r#"{"value":"old"}"#)
    );
    assert!(as_of(&test.session, &doc, conversation_id, retired).await.unwrap().is_none());
    assert_eq!(
        as_of(&test.session, &doc, conversation_id, recreated).await.unwrap().unwrap(),
        json(r#"{"value":"new"}"#)
    );
    test.session.close(&context()).await.unwrap();
    assert!(message(as_of(&test.session, &doc, conversation_id, recreated).await).contains("closed"));
}
