//! Creation, snapshots, adoption, and empty batches.

use std::sync::{Arc, Mutex};

use pi_chord::Json;
use pi_chord::delta::Op;
use pi_durable::*;

use super::support::*;
use super::*;

#[tokio::test]
async fn creates_an_initial_base_on_first_access_and_adopts_it_after_storage_success() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    set_message(&test.session, conversation_id, "hello").await;
    let writes = test.storage.last_commit();
    assert_eq!(writes.len(), 1);
    let StorageWrite::DocumentCreate { record, content } = &writes[0] else { panic!("expected a creation") };
    assert_eq!(record.kind, "test.live");
    assert_eq!(record.scope, DocumentScope::Conversation { conversation_id });
    assert_eq!(record.history, Some(DocumentHistory::Latest));
    assert_eq!(record.fork, Some(DocumentFork::Initial));
    let DocumentContent::Base { version: 1, value: created } = content else { panic!("expected a base") };
    assert_eq!(created, &json(r#"{"message":"hello","items":[],"nested":{"count":0},"other":{"label":"x"}}"#));
    let snapshot = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert_eq!(snapshot, *created);
    flush().await;
    let publication = test.last_publication();
    let published = &document_changes(&publication)[0];
    let DocumentCommitChange::Document { record, value, conversation_id: owner, .. } = published else { panic!() };
    assert_eq!(record.created_at, publication.seq);
    assert!(value.as_ref().unwrap().same_ref(&snapshot));
    assert_eq!(*owner, Some(conversation_id));
    assert!(created.same_ref(value.as_ref().unwrap()));
}

#[tokio::test]
async fn never_creates_on_snapshot_and_returns_none_when_absent() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let before = test.storage.commit_count();
    let ctx = context();
    assert!(test.session.snapshot(&LIVE_DOC, conversation_id, &ctx).await.unwrap().is_none());
    assert!(test.session.snapshot(&COUNTER_DOC, (), &ctx).await.unwrap().is_none());
    assert!(test.session.family_snapshot(&MEMBER_DOC, (), "k", &ctx).await.unwrap().is_none());
    assert_eq!(test.storage.commit_count(), before);
    assert_eq!(test.storage.mints(), 1);
}

#[tokio::test]
async fn returns_shared_immutable_snapshots_and_keeps_prior_revisions_stable() {
    let (test, conversation_id) = setup_live().await;
    let first = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(&first));
    test.session
        .commit(
            move |tx| async move { tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.nested.count = 1.0) },
            &context(),
        )
        .await
        .unwrap();
    let second = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert!(!second.same_ref(&first));
    assert_eq!(first["nested"]["count"], Json::from(0));
    assert_eq!(second["nested"]["count"], Json::from(1));
    // Unchanged subtrees are structurally shared between immutable revisions.
    assert!(second["items"].same_ref(&first["items"]));
    assert!(second["other"].same_ref(&first["other"]));
}

#[tokio::test]
async fn adopts_by_pointer_swap_and_shares_operation_payloads_with_the_published_revision() {
    let (test, conversation_id) = setup_live().await;
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
                    live.other = Label { label: "y".into() };
                    live.items.push("c".into());
                })
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let snapshot = live_snapshot(&test.session, conversation_id).await.unwrap();
    let DocumentCommitChange::Document { value, ops, .. } = &document_changes(&test.last_publication())[0] else {
        panic!()
    };
    assert!(value.as_ref().unwrap().same_ref(&snapshot));
    let writes = test.storage.last_commit();
    let Some(StorageWrite::DocumentChange { content: DocumentContent::Delta { ops: admitted, .. }, .. }) =
        writes.iter().find(|write| matches!(write, StorageWrite::DocumentChange { .. }))
    else {
        panic!("expected a delta")
    };
    // Rust copies the operation array; its payloads stay shared.
    assert_eq!(ops, admitted);
    // The Rust tracker diffs the draft, so replacing `other` with a one-field object sets only `other.label`.
    let set = ops.iter().find(|op| op.verb() == "s").unwrap();
    let Op::Set(path, payload) = set else { panic!() };
    assert_eq!(path.len(), 2);
    assert_eq!(*payload, Json::from("y"));
    let Some(Op::Splice(_, _, _, items)) = ops.iter().find(|op| op.verb() == "p") else { panic!("expected a splice") };
    // Trusted immutability: the Session makes no second copy of operation payloads.
    assert!(items[0].same_ref(&snapshot["items"][2]));
}

#[tokio::test]
async fn copies_assigned_values_per_placement() {
    let (test, conversation_id) = setup_live().await;
    test.session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation_id).await?;
                let mut value = json(r#"{"label":"shared"}"#);
                live.update(|draft| {
                    draft.set("other", value.clone()).unwrap();
                    draft.set("copy", value.clone()).unwrap();
                })?;
                value.set("label", "mutated").unwrap();
                Ok(())
            },
            &context(),
        )
        .await
        .unwrap();
    let snapshot = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert_eq!(snapshot["other"], json(r#"{"label":"shared"}"#));
    assert_eq!(snapshot["copy"], json(r#"{"label":"shared"}"#));
}

#[tokio::test]
async fn suppresses_writes_and_publications_for_empty_batches() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let (commits, published) = (test.storage.commit_count(), test.publication_count());
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
                    live.nested.count = 0.0;
                    live.items.push("z".into());
                    live.items.pop();
                })
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.publication_count(), published);
}

/// Upstream records Astra draft operations, so a shift then unshift is a nonempty batch. The Rust Chord tracker
/// diffs the draft against its base, which proves this change a no-op: it writes and publishes nothing.
#[tokio::test]
async fn writes_nothing_for_a_structural_change_the_tracker_proves_is_a_no_op() {
    let (test, conversation_id) = setup_live().await;
    flush().await;
    let (commits, before) = (test.storage.commit_count(), live_snapshot(&test.session, conversation_id).await);
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&LIVE_DOC, conversation_id).await?.update(|draft| {
                    let items = draft.get_mut("items").unwrap().as_array_mut().unwrap();
                    let first = items.remove(0);
                    items.insert(0, first);
                })
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(test.storage.commit_count(), commits);
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(&before.unwrap()));
}

#[tokio::test]
async fn revokes_escaped_drafts_when_the_callback_settles() {
    let (test, conversation_id) = setup_live().await;
    let escaped = test
        .session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation_id).await?;
                live.edit(|live| live.message = Some("inside".into()))?;
                Ok(live)
            },
            &context(),
        )
        .await
        .unwrap();
    assert!(escaped.value().is_err());
    assert!(escaped.edit(|live| live.message = Some("outside".into())).is_err());
    assert!(escaped.update(|draft| draft.push("outside")).is_err());
    let snapshot = live_snapshot(&test.session, conversation_id).await.unwrap();
    assert_eq!(snapshot["message"], Json::from("inside"));
}

#[tokio::test]
async fn aborts_every_change_when_the_callback_fails() {
    let (test, conversation_id) = setup_live().await;
    let before = live_snapshot(&test.session, conversation_id).await.unwrap();
    let commits = test.storage.commit_count();
    let result = test
        .session
        .commit(
            move |tx| async move {
                let live = tx.doc(&LIVE_DOC, conversation_id).await?;
                let counter = tx.doc(&COUNTER_DOC, ()).await?;
                live.edit(|live| live.message = Some("lost".into()))?;
                counter.edit(|counter| counter.count = 5.0)?;
                Err::<(), _>(DurableError::error("callback failed"))
            },
            &context(),
        )
        .await;
    assert_eq!(message(result), "callback failed");
    assert_eq!(test.storage.commit_count(), commits);
    assert!(live_snapshot(&test.session, conversation_id).await.unwrap().same_ref(&before));
    assert!(test.session.snapshot(&COUNTER_DOC, (), &context()).await.unwrap().is_none());
    set_message(&test.session, conversation_id, "kept").await;
    assert_eq!(live_snapshot(&test.session, conversation_id).await.unwrap()["message"], Json::from("kept"));
}

#[tokio::test]
async fn memoizes_concurrent_duplicate_acquisition_and_initializes_once() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    let (inits, mints) = (LIVE_INIT_COUNT.load(Ordering::SeqCst), test.storage.mints());
    test.session
        .commit(
            move |tx| async move {
                let (first, second) =
                    tokio::join!(tx.doc(&LIVE_DOC, conversation_id), tx.doc(&LIVE_DOC, conversation_id));
                let (first, second) = (first?, second?);
                assert!(first.ptr_eq(&second));
                assert!(tx.doc(&LIVE_DOC, conversation_id).await?.ptr_eq(&first));
                first.edit(|live| live.message = Some("once".into()))
            },
            &context(),
        )
        .await
        .unwrap();
    // Other tests of this binary initialize the same token concurrently.
    assert!(LIVE_INIT_COUNT.load(Ordering::SeqCst) > inits);
    assert_eq!(test.storage.mints(), mints + 1);
    let creates = test.storage.last_commit();
    assert_eq!(creates.iter().filter(|write| matches!(write, StorageWrite::DocumentCreate { .. })).count(), 1);
}

#[tokio::test]
async fn uses_the_first_family_seed_and_ignores_seeds_for_existing_members() {
    let test = open_test_session();
    let seeds = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = seeds.clone();
    let member_doc = define_doc_family(DocFamilySpec::new(
        "test.member",
        1,
        SessionScope,
        move |seed: String| {
            recorded.lock().unwrap().push(seed.clone());
            Member { seed, hits: 0.0 }
        },
        |_, _, _| Ok(false),
    ))
    .unwrap();
    let token = member_doc.clone();
    test.session
        .commit(
            move |tx| async move {
                let first = tx.family_doc(&token, (), "k", &"first".to_owned()).await?;
                let second = tx.family_doc(&token, (), "k", &"second".to_owned()).await?;
                assert!(second.ptr_eq(&first));
                first.edit(|member| member.hits += 1.0)
            },
            &context(),
        )
        .await
        .unwrap();
    let token = member_doc.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.family_doc(&token, (), "k", &"third".to_owned()).await?.edit(|member| member.hits += 1.0)?;
                tx.family_doc(&token, (), "other", &"fourth".to_owned()).await?.edit(|member| member.hits += 1.0)
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(*seeds.lock().unwrap(), vec!["first".to_owned(), "fourth".to_owned()]);
    let ctx = context();
    let member = test.session.family_snapshot(&member_doc, (), "k", &ctx).await.unwrap();
    assert_eq!(member.unwrap(), json(r#"{"seed":"first","hits":2}"#));
    let other = test.session.family_snapshot(&member_doc, (), "other", &ctx).await.unwrap();
    assert_eq!(other.unwrap(), json(r#"{"seed":"fourth","hits":1}"#));
}
