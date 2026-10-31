//! Listener and report errors of upstream `test/harness-storage-failure.test.ts`. The JavaScript-only cases (rejecting
//! listener promises, thenables, foreign realms, unformattable throws, a throwing clock or `onReport`) are not ported:
//! Rust listeners return a `Result`, and `now` cannot fail.

use std::sync::{Arc, Mutex};

use pi_durable::harness::live::LIVE_DOC;
use pi_durable::session::{Tx, WatchEnd};
use pi_durable::storage::MemoryStorage;
use pi_durable::*;

use super::support::context;
use super::{Started, messages, note, started};

// A Rust close listener returns nothing, so only commit listeners can fail here; the close listener half checks
// that every close listener runs.
#[tokio::test]
async fn reports_a_throwing_commit_listener_and_runs_the_others_the_commit_stands() {
    let Started { harness, root, reports, .. } = started(Arc::new(MemoryStorage::new())).await;
    let seen = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let _failing = harness.subscribe_commits(|_, _| Err(DurableError::error("commit listener"))).unwrap();
    let sink = seen.clone();
    let _commit = harness
        .subscribe_commits(move |_, _| {
            sink.lock().unwrap().push("commit");
            Ok(())
        })
        .unwrap();
    let sink = seen.clone();
    let _close = harness.subscribe_close(move || sink.lock().unwrap().push("close")).unwrap();
    note(&root).await.unwrap();
    harness.close(&context()).await.unwrap();
    assert_eq!(*seen.lock().unwrap(), ["commit", "close"]);
    assert_eq!(messages(&reports), ["commit listener"]);
}

#[tokio::test]
async fn reports_a_throwing_watch_listener_which_ends_that_watch() {
    let Started { harness, root, reports, .. } = started(Arc::new(MemoryStorage::new())).await;
    let watch = harness.watch_doc(&LIVE_DOC, root.id, &context()).await.unwrap().unwrap();
    watch.start(|pi_chord::WatchChange { .. }, _| async { Err(DurableError::error("listener broke")) }).unwrap();
    let id = root.id;
    let change =
        move |tx: Tx| async move { tx.doc(&LIVE_DOC, id).await?.edit(|live| live.compactions = Some(Vec::new())) };
    root.commit(change, &context()).await.unwrap();
    let end = watch.closed().await;
    assert!(matches!(&end, WatchEnd::ListenerError(error) if error.to_string() == "listener broke"), "{end:?}");
    assert_eq!(messages(&reports), ["listener broke"]);
    harness.close(&context()).await.unwrap();
}

// Not ported: "keeps feeding the other observers of a view when one throws". A Rust view observer's `advance`
// returns nothing, so it has no error to report.
