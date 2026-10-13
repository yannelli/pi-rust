//! Retirement and recreation of document incarnations.

use pi_chord::Json;
use pi_durable::*;

use super::support::*;
use super::*;

fn first_record_id(test: &TestSession) -> DocumentId {
    change_record(&document_changes(&test.last_publication())[0]).id
}

fn is_live_create(write: &StorageWrite) -> bool {
    matches!(write, StorageWrite::DocumentCreate { record, .. } if record.kind == "test.live")
}

#[tokio::test]
async fn retires_documents_and_creates_a_new_incarnation_at_the_same_address() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let old_id = first_record_id(&test);
    test.session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation_id).await?;
                live.edit(|live| live.message = Some("final".into()))?;
                tx.retire_doc(&LIVE_DOC, conversation_id).await?;
                let replacement = tx.doc(&LIVE_DOC, conversation_id).await?;
                assert!(!replacement.ptr_eq(&live));
                replacement.edit(|live| live.message = Some("new".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 3);
    assert!(writes.iter().any(|w| matches!(w, StorageWrite::DocumentChange { id, .. } if *id == old_id)));
    assert!(writes.contains(&StorageWrite::DocumentRetire { id: old_id }));
    assert!(writes.iter().any(is_live_create));
    flush().await;
    let publication = test.last_publication();
    let changes = document_changes(&publication);
    let DocumentCommitChange::Document { record: retired, value: None, ops, .. } = &changes[0] else { panic!() };
    assert_eq!(retired.id, old_id);
    assert!(ops.is_empty());
    assert_eq!(retired.retired_at, Some(publication.seq));
    let DocumentCommitChange::Document { record: created, value: Some(value), ops, .. } = &changes[1] else { panic!() };
    assert_ne!(created.id, old_id);
    assert_eq!(created.created_at, publication.seq);
    assert_eq!(value["message"], Json::from("new"));
    assert_eq!(value["items"], json("[]"));
    assert!(ops.is_empty());
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(value));

    let retire = move |tx: pi_durable::session::Tx| async move { tx.retire_doc(&LIVE_DOC, conversation_id).await };
    test.session.commit(retire, &context()).await.unwrap();
    assert!(live_snapshot(&test.session, conversation_id).await.is_none());
    test.session.unload_documents().await.unwrap();
    assert!(live_snapshot(&test.session, conversation_id).await.is_none());
    // Retiring an absent address is a no-op.
    let commits = test.storage.commit_count();
    test.session.commit(retire, &context()).await.unwrap();
    assert_eq!(test.storage.commit_count(), commits);
}

#[tokio::test]
async fn retires_without_acquisition_and_recreates_both_existing_and_absent_addresses() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let old_id = first_record_id(&test);
    test.session.unload_documents().await.unwrap();
    test.session
        .commit(
            move |tx| async move {
                let retired = tx.retire_doc(&LIVE_DOC, conversation_id);
                let replacement = tx.doc(&LIVE_DOC, conversation_id);
                let (live, retired) = tokio::join!(replacement, retired);
                retired?;
                live?.edit(|live| live.message = Some("replacement".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 2);
    assert!(writes.contains(&StorageWrite::DocumentRetire { id: old_id }));
    assert!(writes.iter().any(is_live_create));

    test.session
        .commit(
            |tx| async move {
                let retired = tx.retire_family_doc(&MEMBER_DOC, (), "absent");
                let replacement = tx.family_doc(&MEMBER_DOC, (), "absent", &"seed".to_owned());
                let (member, retired) = tokio::join!(replacement, retired);
                retired?;
                member?.edit(|member| member.hits = 1.0)
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert!(!writes.iter().any(|w| matches!(w, StorageWrite::DocumentRetire { .. })));
    assert_eq!(writes.iter().filter(|w| matches!(w, StorageWrite::DocumentCreate { .. })).count(), 1);
    let member = test.session.family_snapshot(&MEMBER_DOC, (), "absent", &context()).await.unwrap();
    assert_eq!(member.unwrap(), json(r#"{"seed":"seed","hits":1}"#));
}

#[tokio::test]
async fn retires_the_existing_incarnation_when_retirement_races_a_pending_acquisition() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let old_id = first_record_id(&test);
    test.session
        .commit(
            move |tx| async move {
                let acquired = tx.doc(&LIVE_DOC, conversation_id);
                let retired = tx.retire_doc(&LIVE_DOC, conversation_id);
                acquired.await?.edit(|live| live.message = Some("final".into()))?;
                retired.await
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 2);
    assert!(writes.iter().any(|w| matches!(
        w,
        StorageWrite::DocumentChange { id, content: DocumentContent::Delta { .. } } if *id == old_id
    )));
    assert!(writes.contains(&StorageWrite::DocumentRetire { id: old_id }));
    assert!(live_snapshot(&test.session, conversation_id).await.is_none());

    set_message(&test.session, conversation_id, "second").await;
    flush().await;
    let second_id = first_record_id(&test);
    test.session
        .commit(
            move |tx| async move {
                let acquired = tx.doc(&LIVE_DOC, conversation_id);
                let retired = tx.retire_doc(&LIVE_DOC, conversation_id);
                let recreated = tx.doc(&LIVE_DOC, conversation_id);
                let (old, fresh, retired) = tokio::join!(acquired, recreated, retired);
                retired?;
                let (old, fresh) = (old?, fresh?);
                assert!(!fresh.ptr_eq(&old));
                fresh.edit(|live| live.message = Some("third".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 2);
    assert!(writes.contains(&StorageWrite::DocumentRetire { id: second_id }));
    assert!(writes.iter().any(is_live_create));
    assert_eq!(live_snapshot(&test.session, conversation_id).await.unwrap()["message"], Json::from("third"));
}
