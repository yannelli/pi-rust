//! Watch lifetime: contexts, retirement, Session close, stop, cancellation, listener failure, and reloads.

use std::sync::{Arc, Mutex};

use pi_chord::{AbortController, BACKGROUND_CONTEXT, ChordError, Context, create_context_key, with_abort_signal};
use pi_chord::{with_cancel, with_context_value};
use pi_durable::DurableError;
use pi_durable::session::WatchEnd;

use super::support::*;
use super::*;

/// Start `watch` with a listener that records its context, signals entry, and waits for release.
fn hold(watch: &CommittedWatch) -> (Deferred, Deferred, Arc<Mutex<Option<Context>>>) {
    let (entered, release) = (Deferred::default(), Deferred::default());
    let delivered: Arc<Mutex<Option<Context>>> = Arc::default();
    let (gate_in, gate_out, sink) = (entered.clone(), release.clone(), delivered.clone());
    watch
        .start(move |_, _, context| {
            *sink.lock().unwrap() = Some(context);
            let (gate_in, gate_out) = (gate_in.clone(), gate_out.clone());
            async move {
                gate_in.resolve();
                gate_out.wait().await;
                Ok(())
            }
        })
        .unwrap();
    (entered, release, delivered)
}

#[tokio::test]
async fn preserves_commit_context_values_without_inheriting_producer_cancellation() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let key = create_context_key::<String>("watch-test");
    let parent = AbortController::new();
    let base = with_abort_signal(&parent.signal(), &BACKGROUND_CONTEXT);
    let commit_context = with_context_value(&key, "newest-commit".to_owned(), &base);
    let (entered, release, delivered) = hold(&watch);
    test.session
        .commit(
            |tx| async move { tx.doc(&STATE_DOC, ()).await?.update(|s| s.set("value", 1))?.map_err(Into::into) },
            &commit_context,
        )
        .await
        .map_err(|e: DurableError| e.to_string())
        .unwrap();
    entered.wait().await;
    let delivery = delivered.lock().unwrap().clone().unwrap();
    assert_eq!(delivery.value(&key).map(String::as_str), Some("newest-commit"));
    assert!(delivery.abort_signal().is_none());
    parent.abort(Some(ChordError::error("caller finished")));
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
    assert!(delivery.abort_signal().is_none());
    release.resolve();
}

#[tokio::test]
async fn keeps_earlier_immutable_revisions_stable() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let initial = watch.value();
    let frames = record(&watch);
    set_value(&test.session, 1).await;
    flush().await;
    let delivered = frames.lock().unwrap()[0].0.clone();
    assert!(delivered.same_ref(&watch.value()));
    assert!(!delivered.same_ref(&initial));
    assert!(delivered["retained"].same_ref(&initial["retained"]));
    assert_eq!(value_of(&initial), 0);
    watch.stop().await;
}

#[tokio::test]
async fn delivers_retirement_and_does_not_follow_recreation() {
    let test = create_state().await;
    let old = watch_state(&test.session).await;
    let frames = record(&old);
    test.session
        .commit(
            |tx| async move {
                tx.retire_doc(&STATE_DOC, ()).await?;
                tx.doc(&STATE_DOC, ()).await?.update(|s| s.set("value", 10))?.map_err(Into::into)
            },
            &context(),
        )
        .await
        .unwrap();
    assert!(matches!(old.closed().await, WatchEnd::Retired));
    assert_eq!(frames.lock().unwrap().iter().map(|f| f.0.clone()).collect::<Vec<_>>(), [Json::Null]);
    assert!(old.value().is_null());
    let replacement = watch_state(&test.session).await;
    assert_eq!(value_of(&replacement.value()), 10);
    set_value(&test.session, 11).await;
    flush().await;
    assert!(old.value().is_null());
    replacement.stop().await;
}

#[tokio::test]
async fn session_close_discards_retirement_buffered_before_start() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let baseline = watch.value();
    retire_state(&test.session).await;
    test.session.close(&context()).await.unwrap();
    assert!(matches!(watch.closed().await, WatchEnd::SessionClosed));
    assert!(watch.value().same_ref(&baseline));
    assert!(message(watch.start(|_, _, _| async { Ok(()) })).contains("stopped"));
}

#[tokio::test]
async fn session_close_discards_retirement_behind_an_in_flight_callback() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let (entered, release) = (Deferred::default(), Deferred::default());
    let values: Arc<Mutex<Vec<Json>>> = Arc::default();
    let (sink, gate_in, gate_out) = (values.clone(), entered.clone(), release.clone());
    watch
        .start(move |value, _, _| {
            sink.lock().unwrap().push(value["value"].clone());
            let (gate_in, gate_out) = (gate_in.clone(), gate_out.clone());
            async move {
                gate_in.resolve();
                gate_out.wait().await;
                Ok(())
            }
        })
        .unwrap();
    set_value(&test.session, 1).await;
    entered.wait().await;
    retire_state(&test.session).await;
    test.session.close(&context()).await.unwrap();
    assert!(matches!(watch.closed().await, WatchEnd::SessionClosed));
    release.resolve();
    flush().await;
    assert_eq!(*values.lock().unwrap(), [Json::from(1)]);
}

#[tokio::test]
async fn supports_idempotent_stop_and_rejects_repeated_or_late_start() {
    let test = create_state().await;
    let started = watch_state(&test.session).await;
    started.start(|_, _, _| async { Ok(()) }).unwrap();
    assert!(message(started.start(|_, _, _| async { Ok(()) })).contains("already started"));
    let (first, second) = (started.stop(), started.stop());
    assert!(matches!(first.await, WatchEnd::Stopped));
    assert!(matches!(second.await, WatchEnd::Stopped));
    let stopped = watch_state(&test.session).await;
    stopped.stop().await;
    assert!(message(stopped.start(|_, _, _| async { Ok(()) })).contains("stopped"));
}

#[tokio::test]
async fn cancels_acquisition_without_leaking_a_registered_watch() {
    let test = create_state().await;
    test.session.unload_documents().await.unwrap();
    let gate = test.storage.hold_find_document();
    let (child, controller) = with_cancel(&context());
    let acquisition = test.session.watch_doc(&STATE_DOC, (), &child);
    gate.entered().await;
    controller.abort(Some(ChordError::error("cancel acquisition")));
    gate.release();
    assert!(message(acquisition.await).contains("cancel acquisition"));
    test.session.close(&context()).await.unwrap();
}

#[tokio::test]
async fn cancels_future_delivery_without_aborting_an_in_flight_callback() {
    let test = create_state().await;
    let (child, controller) = with_cancel(&context());
    let watch = test.session.watch_doc(&STATE_DOC, (), &child).await.unwrap().unwrap();
    let (entered, release, delivered) = hold(&watch);
    set_value(&test.session, 1).await;
    entered.wait().await;
    controller.abort(None);
    assert!(matches!(watch.closed().await, WatchEnd::Cancelled));
    assert!(delivered.lock().unwrap().as_ref().unwrap().abort_signal().is_none());
    release.resolve();
}

#[tokio::test]
async fn session_close_stops_future_delivery_without_joining_an_in_flight_callback() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let (entered, release, delivered) = hold(&watch);
    set_value(&test.session, 1).await;
    entered.wait().await;
    test.session.close(&context()).await.unwrap();
    assert!(delivered.lock().unwrap().as_ref().unwrap().abort_signal().is_none());
    assert!(matches!(watch.closed().await, WatchEnd::SessionClosed));
    release.resolve();
}

#[tokio::test]
async fn settles_listener_failure_on_only_the_affected_watch() {
    let test = create_state().await;
    let failed = watch_state(&test.session).await;
    let healthy = watch_state(&test.session).await;
    failed.start(|_, _, _| async { Err(DurableError::error("listener failed")) }).unwrap();
    let frames = record(&healthy);
    set_value(&test.session, 1).await;
    let end = failed.closed().await;
    assert_eq!(end.reason(), "listener_error");
    assert_eq!(end.error().unwrap().to_string(), "listener failed");
    flush().await;
    assert_eq!(frames.lock().unwrap().len(), 1);
    healthy.stop().await;
}

#[tokio::test]
async fn hydrates_migration_without_writing_and_observes_the_later_exact_edit() {
    let old = define_doc(DocSpec::new("watch.migration", 1, SessionScope, || json(r#"{"value":3}"#))).unwrap();
    let current = define_doc(
        DocSpec::new("watch.migration", 2, SessionScope, || json(r#"{"value":0,"migrated":false}"#))
            .migrate(|value, _| Ok(json(&format!(r#"{{"value":{},"migrated":true}}"#, value["value"])))),
    )
    .unwrap();
    let test = open_test_session();
    test.session.commit(move |tx| async move { tx.doc(&old, ()).await.map(drop) }, &context()).await.unwrap();
    test.session.unload_documents().await.unwrap();
    let commits = test.storage.commit_count();
    let watch = test.session.watch_doc(&current, (), &context()).await.unwrap().unwrap();
    assert_eq!(watch.value(), json(r#"{"value":3,"migrated":true}"#));
    assert_eq!(test.storage.commit_count(), commits);
    let frames = record(&watch);
    let token = current.clone();
    test.session.commit(move |tx| async move { tx.doc(&token, ()).await.map(drop) }, &context()).await.unwrap();
    flush().await;
    assert!(frames.lock().unwrap().is_empty());
    let token = current.clone();
    test.session
        .commit(
            move |tx| async move { tx.doc(&token, ()).await?.update(|s| s.set("value", 4))?.map_err(Into::into) },
            &context(),
        )
        .await
        .unwrap();
    flush().await;
    assert_eq!(frames.lock().unwrap().iter().map(|f| value_of(&f.0)).collect::<Vec<_>>(), [4]);
    watch.stop().await;
}

#[tokio::test]
async fn continues_after_the_tracker_cache_unloads() {
    let test = create_state().await;
    let watch = watch_state(&test.session).await;
    let baseline = watch.value();
    let reads = test.storage.document_reads();
    test.session.unload_documents().await.unwrap();
    let reloaded = test.session.snapshot(&STATE_DOC, (), &context()).await.unwrap().unwrap();
    assert_eq!(reloaded, baseline);
    assert!(test.storage.document_reads() > reads);
    watch.start(|_, _, _| async { Ok(()) }).unwrap();
    set_value(&test.session, 7).await;
    flush().await;
    assert_eq!(value_of(&watch.value()), 7);
    watch.stop().await;
}
