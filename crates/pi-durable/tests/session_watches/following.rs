//! A watch follows its address: retirements, recreations, other addresses of the same kind, and what it keeps.

use std::sync::{Arc, Weak};

use pi_chord::delta::Op;
use pi_durable::session::Tx;
use pi_durable::session::WatchEnd;
use pi_durable::{ConversationId, StorageWrite};

use super::support::*;
use super::*;
use crate::support::{ValueCall, on_value};

fn text(text: &str) -> Json {
    json(&format!(r#"{{"text":"{text}"}}"#))
}

static NOTES_DOC: LazyLock<DocToken<Json, LatestConversation>> = LazyLock::new(|| {
    let scope = LatestConversation(LatestFork::Initial);
    define_doc(DocSpec::new("watch.scoped-notes", 1, scope, || text(""), |_, _, _| Ok(false))).unwrap()
});

static MEMBERS_DOC: LazyLock<DocFamilyToken<Json, (), SessionScope>> = LazyLock::new(|| {
    define_doc_family(DocFamilySpec::new("watch.members", 1, SessionScope, |()| text(""), |_, _, _| Ok(false))).unwrap()
});

fn set_text(state: &mut Json, value: &str) -> pi_chord::ChordResult<()> {
    state.set("text", value)
}

#[tokio::test]
async fn reports_a_change_and_a_retirement_in_one_commit_as_the_retirement_with_the_value_it_was_given_before() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let before = watch.value();
    let changes = record_changes(&watch);
    test.session
        .commit(
            |tx| async move {
                tx.doc(&STATE_DOC, ()).await?.update(|s| s.set("value", 5))??;
                tx.retire_doc(&STATE_DOC, ()).await
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    // Storage got the change, then the retirement; the commit publishes the retirement only.
    let writes = test.storage.last_commit();
    assert!(matches!(writes.as_slice(), [StorageWrite::DocumentChange { .. }, StorageWrite::DocumentRetire { .. }]));
    assert_eq!(*changes.lock().unwrap(), [ValueCall { value: before.clone(), ops: Vec::new(), retired: true }]);
    assert!(changes.lock().unwrap()[0].value.same_ref(&before));
    watch.stop().await;
}

#[tokio::test]
async fn ignores_the_same_kind_in_another_conversation_and_under_another_family_key() {
    let test = open_test_session();
    let session = &test.session;
    let watched = create_conversation(session).await;
    let other = create_conversation(session).await;
    let write = move |id: ConversationId, key: &'static str, value: &'static str| {
        session.commit(
            move |tx| async move {
                tx.doc(&NOTES_DOC, id).await?.update(|s| set_text(s, value))??;
                tx.family_doc(&MEMBERS_DOC, (), key, &()).await?.update(|s| set_text(s, value))?.map_err(Into::into)
            },
            &context(),
        )
    };
    write(watched, "watched", "a").await.unwrap();
    let notes = session.watch_doc(&NOTES_DOC, watched, &context()).await.unwrap().unwrap();
    let member = session.family_watch_doc(&MEMBERS_DOC, (), "watched", &context()).await.unwrap().unwrap();
    let (notes_changes, member_changes) = (record_changes(&notes), record_changes(&member));
    // The same kinds at other addresses: created, changed, retired, and created again.
    write(other, "other", "other").await.unwrap();
    session
        .commit(
            move |tx| async move {
                tx.retire_doc(&NOTES_DOC, other).await?;
                tx.retire_family_doc(&MEMBERS_DOC, (), "other").await
            },
            &context(),
        )
        .await
        .unwrap();
    write(other, "other", "again").await.unwrap();
    flush().await;
    assert!(notes_changes.lock().unwrap().is_empty());
    assert!(member_changes.lock().unwrap().is_empty());
    assert_eq!(session.document_cache_sizes().attached, 2);
    notes.stop().await;
    member.stop().await;
    session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn takes_the_version_of_a_recreation_written_by_a_newer_definition_then_its_ops() {
    let old = define_doc(DocSpec::new(
        "watch.recreated-version",
        1,
        SessionScope,
        || json(r#"{"value":1}"#),
        |_, _, _| Ok(false),
    ))
    .unwrap();
    let initial = || json(r#"{"value":0,"migrated":false}"#);
    let current = define_doc(
        DocSpec::new("watch.recreated-version", 2, SessionScope, initial, |_, _, _| Ok(false))
            .migrate(|value, _| Ok(json(&format!(r#"{{"value":{},"migrated":true}}"#, value["value"])))),
    )
    .unwrap();
    let test = open_test_session();
    let token = old.clone();
    test.session.commit(move |tx| async move { tx.doc(&token, ()).await.map(drop) }, &context()).await.unwrap();
    let watch = test.session.watch_doc(&old, (), &context()).await.unwrap().unwrap();
    let changes = record_changes(&watch);
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.retire_doc(&token, ()).await?;
                tx.doc(&token, ()).await?.update(|s| s.set("value", 5))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move { tx.doc(&token, ()).await?.update(|s| s.set("value", 6))?.map_err(Into::into) },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let recreated = json(r#"{"value":5,"migrated":false}"#);
    let expected = [
        ValueCall { value: recreated.clone(), ops: vec![Op::Replace(recreated)], retired: true },
        ValueCall {
            value: json(r#"{"value":6,"migrated":false}"#),
            ops: ops(r#"[["s",["value"],6]]"#),
            retired: false,
        },
    ];
    assert_eq!(*changes.lock().unwrap(), expected);
    watch.stop().await;
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn keeps_a_watch_of_a_retired_conversation_document_until_it_is_stopped_then_nothing_of_it() {
    let test = open_test_session();
    let conversation_id = create_conversation(&test.session).await;
    // A started watch of a document that is then retired; `stop` says whether to stop it before returning.
    let acquire = async |stop: bool| -> (DocumentWatch, Weak<()>) {
        let session = &test.session;
        let write = move |tx: Tx| async move {
            tx.doc(&NOTES_DOC, conversation_id).await?.update(|s| set_text(s, "kept"))?.map_err(Into::into)
        };
        session.commit(write, &context()).await.unwrap();
        let watch = session.watch_doc(&NOTES_DOC, conversation_id, &context()).await.unwrap().unwrap();
        let sentinel = Arc::new(());
        let listener = Arc::downgrade(&sentinel);
        watch
            .start(on_value(move |ValueCall { .. }, _| {
                let _held = sentinel.clone();
                async { Ok(()) }
            }))
            .unwrap();
        let retire = move |tx: Tx| async move { tx.retire_doc(&NOTES_DOC, conversation_id).await };
        session.commit(retire, &context()).await.unwrap();
        flush().await;
        if stop {
            watch.stop().await;
        }
        (watch, listener)
    };
    // A watch follows its address, so a forgotten one stays attached until it is stopped or the Session ends.
    let (forgotten, listener) = acquire(false).await;
    flush().await;
    assert!(listener.upgrade().is_some());
    assert!(forgotten.value()["text"] == Json::from("kept"));
    assert!(matches!(forgotten.stop().await, WatchEnd::Stopped));

    let (stopped, listener) = acquire(true).await;
    flush().await;
    assert!(listener.upgrade().is_none());
    assert!(matches!(stopped.closed().await, WatchEnd::Stopped));
    test.session.close(&context()).await.unwrap();
}
