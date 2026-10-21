//! Session document checkpoints.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;

use super::support::*;
use super::*;

#[tokio::test]
async fn calls_checkpoint_when_with_access_to_its_definition() {
    // Upstream binds `this` to the definition; a Rust predicate captures what it needs instead.
    let first = json(r#"{"count":0}"#);
    let spec = session_doc_with("checkpoint.receiver", 1, r#"{"count":0}"#, move |value, _, _| {
        Ok(value["count"].as_i64() == Some(first["count"].as_i64().unwrap() + 2))
    });
    let doc = define_doc(spec).unwrap();
    let test = open_test_session();
    for count in 0..=2 {
        set_field(&test.session, &doc, (), "count", Json::from(count)).await.unwrap();
    }
    assert_eq!(change_kinds(&test.storage.commits()), ["delta", "base"]);
}

type Calls = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

fn recording(kind: &str, initial: &str, decision: bool, calls: &Calls) -> SessionDoc {
    let sink = calls.clone();
    define_doc(session_doc_with(kind, 1, initial, move |value, ops, _| {
        sink.lock().unwrap().push((value.clone(), ops.to_vec()));
        Ok(decision)
    }))
    .unwrap()
}

#[tokio::test]
async fn selects_bases_only_for_nonempty_ordinary_batches_and_passes_the_exact_prepared_revision_and_ops() {
    let (calls, false_calls): (Calls, Calls) = Default::default();
    let base_doc = recording("checkpoint.base", r#"{"items":[]}"#, true, &calls);
    let delta_doc = recording("checkpoint.delta", r#"{"count":0}"#, false, &false_calls);
    let default_doc = define_doc(session_doc("checkpoint.default", 1, r#"{"count":0}"#)).unwrap();
    let test = open_test_session();
    let tokens = (base_doc.clone(), delta_doc.clone(), default_doc.clone());
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&tokens.0, ()).await?;
                tx.doc(&tokens.1, ()).await?;
                tx.doc(&tokens.2, ()).await.map(drop)
            },
            &context(),
        )
        .await
        .unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert!(false_calls.lock().unwrap().is_empty());
    let writes = document_writes(&test.storage.last_commit());
    assert!(writes.iter().all(|w| matches!(w, StorageWrite::DocumentCreate { .. }) && content_kind(w) == Some("base")));

    let tokens = (base_doc.clone(), delta_doc.clone(), default_doc.clone());
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&tokens.0, ()).await?.update(|d| d.get_mut("items").unwrap().push("x"))??;
                tx.doc(&tokens.1, ()).await?.update(|d| d.set("count", 1))??;
                tx.doc(&tokens.2, ()).await?.update(|d| d.set("count", 1))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    let writes: Vec<_> =
        test.storage.last_commit().into_iter().filter(|w| matches!(w, StorageWrite::DocumentChange { .. })).collect();
    assert_eq!(writes.iter().filter_map(content_kind).collect::<Vec<_>>(), ["base", "delta", "delta"]);
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(false_calls.lock().unwrap().len(), 1);
    let snapshot = snapshot(&test.session, &base_doc).await.unwrap().unwrap();
    let delta_snapshot = snapshot_of(&test, &delta_doc).await;
    let published = document_changes(&test.last_publication());
    let (DocumentCommitChange::Document { ops: base_ops, .. }, DocumentCommitChange::Document { ops: delta_ops, .. }) =
        (&published[0], &published[1])
    else {
        panic!()
    };
    let calls = calls.lock().unwrap();
    let false_calls = false_calls.lock().unwrap();
    assert!(calls[0].0.same_ref(&snapshot));
    assert_eq!(&calls[0].1, base_ops);
    assert!(false_calls[0].0.same_ref(&delta_snapshot));
    assert_eq!(&false_calls[0].1, delta_ops);
    let StorageWrite::DocumentChange { content: DocumentContent::Delta { ops, .. }, .. } = &writes[1] else { panic!() };
    assert_eq!(ops, &false_calls[0].1);
    let StorageWrite::DocumentChange { content: DocumentContent::Base { value, .. }, .. } = &writes[0] else {
        panic!()
    };
    assert!(value.same_ref(&snapshot));
}

async fn snapshot_of(test: &TestSession, token: &SessionDoc) -> Json {
    snapshot(&test.session, token).await.unwrap().unwrap()
}

#[tokio::test]
async fn passes_the_stored_delta_count_since_the_newest_base_including_after_unload_and_version_bases() {
    let seen: Arc<Mutex<Vec<u32>>> = Arc::default();
    let sink = seen.clone();
    let v1 = define_doc(session_doc_with("checkpoint.deltas-since-base", 1, r#"{"count":0}"#, move |_, _, info| {
        sink.lock().unwrap().push(info.deltas_since_base);
        Ok(info.deltas_since_base >= 2)
    }))
    .unwrap();
    let sink = seen.clone();
    let v2 = define_doc(
        session_doc_with("checkpoint.deltas-since-base", 2, r#"{"count":0}"#, move |_, _, info| {
            sink.lock().unwrap().push(info.deltas_since_base);
            Ok(false)
        })
        .migrate(|value, _| Ok(json(&format!(r#"{{"count":{}}}"#, value["count"])))),
    )
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &v1, ()).await.unwrap();
    for _ in 0..3 {
        increment(&test.session, &v1).await;
    }
    test.session.unload_documents().await.unwrap();
    increment(&test.session, &v1).await;
    assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 0]);
    let commits = test.storage.commits();
    assert_eq!(change_kinds(&commits[commits.len() - 4..]), ["delta", "delta", "base", "delta"]);
    // A required version base resets the count without calling the predicate.
    increment(&test.session, &v2).await;
    increment(&test.session, &v2).await;
    assert_eq!(*seen.lock().unwrap(), [0, 1, 2, 0, 0]);
    let commits = test.storage.commits();
    let StorageWrite::DocumentChange { content, .. } = &commits[commits.len() - 2][0] else { panic!() };
    assert!(matches!(content, DocumentContent::Base { version: 2, .. }));
}

/// Upstream's shift then unshift is a nonempty structural no-op that consults the predicate. The Rust tracker
/// diffs drafts, so that change is empty here; only the empty-batch half of the case applies.
#[tokio::test]
async fn skips_the_predicate_for_empty_batches() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let doc = define_doc(session_doc_with("checkpoint.no-op", 1, r#"{"items":["a","b"]}"#, move |_, _, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }))
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &doc, ()).await.unwrap();
    let commits = test.storage.commit_count();
    let token = doc.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|d| {
                    let items = d.get_mut("items").unwrap().as_array_mut().unwrap();
                    items.push("x".into());
                    items.pop();
                    let first = items.remove(0);
                    items.insert(0, first);
                })
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(test.storage.commit_count(), commits);
}

#[tokio::test]
async fn rolls_back_every_prepared_document_when_a_checkpoint_predicate_throws() {
    let throw_checkpoint = Arc::new(AtomicBool::new(true));
    let first_calls = Arc::new(AtomicUsize::new(0));
    let counter = first_calls.clone();
    let first = define_doc(session_doc_with("checkpoint.rollback.first", 1, r#"{"count":0}"#, move |_, _, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }))
    .unwrap();
    let fail = throw_checkpoint.clone();
    let second = define_doc(session_doc_with("checkpoint.rollback.second", 1, r#"{"count":0}"#, move |_, _, _| {
        if fail.load(Ordering::SeqCst) {
            return Err(DurableError::error("checkpoint failed"));
        }
        Ok(false)
    }))
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &first, ()).await.unwrap();
    touch(&test.session, &second, ()).await.unwrap();
    flush().await;
    let (before_first, before_second) = (snapshot_of(&test, &first).await, snapshot_of(&test, &second).await);
    let (commits, published) = (test.storage.commit_count(), test.publication_count());
    let both = |a: i64, b: i64| {
        let (first, second) = (first.clone(), second.clone());
        move |tx: session::Tx| async move {
            tx.doc(&first, ()).await?.update(|d| d.set("count", a))??;
            tx.doc(&second, ()).await?.update(|d| d.set("count", b))?.map_err(Into::into)
        }
    };
    let failed = test.session.commit(both(1, 2), &context()).await;
    assert_eq!(message(failed), "checkpoint failed");
    flush().await;
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(test.storage.commit_count(), commits);
    assert_eq!(test.publication_count(), published);
    assert!(snapshot_of(&test, &first).await.same_ref(&before_first));
    assert!(snapshot_of(&test, &second).await.same_ref(&before_second));
    throw_checkpoint.store(false, Ordering::SeqCst);
    test.session.commit(both(3, 4), &context()).await.unwrap();
    assert_eq!(snapshot_of(&test, &first).await, json(r#"{"count":3}"#));
    assert_eq!(snapshot_of(&test, &second).await, json(r#"{"count":4}"#));
}

#[tokio::test]
async fn persists_repeated_false_decisions_as_deltas_and_replays_the_complete_tail() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let doc = define_doc(session_doc_with("checkpoint.tail", 1, r#"{"values":[]}"#, move |_, _, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(false)
    }))
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &doc, ()).await.unwrap();
    for value in 1..=8 {
        let token = doc.clone();
        test.session
            .commit(
                move |tx| async move {
                    tx.doc(&token, ()).await?.update(|d| d.get_mut("values").unwrap().push(value))?.map_err(Into::into)
                },
                &context(),
            )
            .await
            .unwrap();
    }
    assert_eq!(change_kinds(&test.storage.commits()), ["delta"; 8]);
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    test.session.unload_documents().await.unwrap();
    assert_eq!(snapshot_of(&test, &doc).await, json(r#"{"values":[1,2,3,4,5,6,7,8]}"#));
}

#[tokio::test]
async fn keeps_a_prepared_root_replacement_as_a_delta_when_the_predicate_is_false() {
    let mut initial = pi_chord::JsonMap::new();
    for index in 0..4_100 {
        initial.insert(format!("field{index}"), 0);
    }
    let initial = Json::object(initial);
    let spec =
        DocSpec::new("checkpoint.root-replacement", 1, SessionScope, move || initial.clone(), |_, _, _| Ok(false));
    let doc = define_doc(spec).unwrap();
    let test = open_test_session();
    touch(&test.session, &doc, ()).await.unwrap();
    let token = doc.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|d| {
                    for index in 0..4_100 {
                        d.set(format!("field{index}"), 1).unwrap();
                    }
                })
            },
            &context(),
        )
        .await
        .unwrap();
    let Some(DocumentContent::Delta { ops, .. }) = first_content(&test) else { panic!("expected a delta") };
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0].verb(), "r");
    test.session.unload_documents().await.unwrap();
    assert_eq!(snapshot_of(&test, &doc).await["field4099"], Json::from(1));
}

#[tokio::test]
async fn uses_ordinary_checkpoint_selection_before_retirement() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let doc = define_doc(session_doc_with("checkpoint.retire", 1, r#"{"count":0}"#, move |_, _, _| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }))
    .unwrap();
    let test = open_test_session();
    touch(&test.session, &doc, ()).await.unwrap();
    let token = doc.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|d| d.set("count", 1))??;
                tx.retire_doc(&token, ()).await
            },
            &context(),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let writes = document_writes(&test.storage.last_commit());
    assert_eq!(writes.len(), 2);
    assert_eq!(content_kind(&writes[0]), Some("base"));
    assert!(matches!(writes[1], StorageWrite::DocumentRetire { .. }));
}
