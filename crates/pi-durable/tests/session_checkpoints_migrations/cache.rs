//! Session tracker cache across definition versions.

use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;

use super::support::*;
use super::*;

fn v1_doc() -> SessionDoc {
    define_doc(session_doc("cache.versioned", 1, r#"{"name":"first"}"#)).unwrap()
}

fn v2_doc() -> SessionDoc {
    let spec = session_doc("cache.versioned", 2, r#"{"names":[]}"#);
    define_doc(spec.migrate(|value, _| Ok(json(&format!(r#"{{"names":[{}]}}"#, value["name"]))))).unwrap()
}

async fn push_second(test: &TestSession, v2: &SessionDoc) {
    let token = v2.clone();
    test.session
        .commit(
            move |tx| async move {
                tx.doc(&token, ()).await?.update(|d| d.get_mut("names").unwrap().push("second"))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn migrates_a_document_cached_by_an_older_token_without_unloading() {
    let (v1, v2) = (v1_doc(), v2_doc());
    let test = open_test_session();
    touch(&test.session, &v1, ()).await.unwrap();
    assert_eq!(snapshot(&test.session, &v1).await.unwrap().unwrap(), json(r#"{"name":"first"}"#));
    // Reloaded extension code accesses the still-cached document with a newer token.
    assert_eq!(snapshot(&test.session, &v2).await.unwrap().unwrap(), json(r#"{"names":["first"]}"#));
    push_second(&test, &v2).await;
    let writes = document_writes(&test.storage.last_commit());
    let StorageWrite::DocumentChange { content: DocumentContent::Base { version: 2, value }, .. } = &writes[0] else {
        panic!("expected a version base")
    };
    assert_eq!(*value, json(r#"{"names":["first","second"]}"#));
    assert!(message(snapshot(&test.session, &v1).await).contains("newer version 2"));
}

#[tokio::test]
async fn sends_observers_of_an_older_shape_a_root_replacement_after_a_newer_token_writes() {
    let (v1, v2) = (v1_doc(), v2_doc());
    let test = open_test_session();
    touch(&test.session, &v1, ()).await.unwrap();
    let state = test.session.document_state(&v1, (), &context()).await.unwrap().unwrap();
    let watch = test.session.watch_doc(&v1, (), &context()).await.unwrap().unwrap();
    let frames: Arc<Mutex<Vec<Vec<Op>>>> = Arc::default();
    let delivered = Deferred::default();
    let (sink, done) = (frames.clone(), delivered.clone());
    watch
        .start(move |_, ops, _| {
            sink.lock().unwrap().push(ops);
            done.resolve();
            async { Ok(()) }
        })
        .unwrap();
    push_second(&test, &v2).await;
    delivered.wait().await;
    flush().await;
    let expected = json(r#"{"names":["first","second"]}"#);
    assert_eq!(state.value(), expected);
    assert_eq!(watch.value(), expected);
    assert_eq!(*frames.lock().unwrap(), vec![vec![Op::Replace(expected)]]);
    state.dispose().unwrap();
    watch.stop().await;
}

#[tokio::test]
async fn serves_an_older_token_from_storage_after_a_newer_token_migrated_only_in_memory() {
    let (v1, v2) = (v1_doc(), v2_doc());
    let test = open_test_session();
    touch(&test.session, &v1, ()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    assert_eq!(snapshot(&test.session, &v2).await.unwrap().unwrap(), json(r#"{"names":["first"]}"#));
    assert_eq!(snapshot(&test.session, &v1).await.unwrap().unwrap(), json(r#"{"name":"first"}"#));
    set_field(&test.session, &v1, (), "name", Json::from("renamed")).await.unwrap();
    assert_eq!(snapshot(&test.session, &v2).await.unwrap().unwrap(), json(r#"{"names":["renamed"]}"#));
}
