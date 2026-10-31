//! How a watch ends, and what it keeps alive afterwards.

use std::sync::{Arc, Mutex, Weak};

use pi_chord::{AbortController, ChordError, Json, Watch, WatchEnd, json};

use super::support::Gate;
use super::*;

#[tokio::test]
async fn stops_idempotently_and_rejects_a_repeated_or_late_start() {
    let (watch, recorded) = recorded_watch(json!(0));
    watch.start(|_, _| async { Ok(()) }).unwrap();
    let repeated = watch.start(|_, _| async { Ok(()) }).unwrap_err();
    assert_eq!(repeated.to_string(), "Watch is already started");
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
    assert_eq!(recorded.detached(), 1);

    let (late, _) = recorded_watch(json!(0));
    late.stop().await;
    assert_eq!(late.start(|_, _| async { Ok(()) }).unwrap_err().to_string(), "Watch is stopped");
}

#[tokio::test]
async fn ends_ended_or_failed_from_its_source_dropping_pending_frames() {
    let (watch, _, frames) = collecting_watch(json!(0));
    watch.advance(frame(json!(1), replace(json!(1))), context());
    watch.end(WatchEnd::Ended);
    flush().await;
    assert!(frames.lock().unwrap().is_empty());
    assert!(matches!(watch.closed().await, WatchEnd::Ended));

    let (failed, _) = recorded_watch(json!(0));
    failed.end(WatchEnd::Failed(ChordError::error("source failed")));
    failed.end(WatchEnd::Ended);
    let end = failed.closed().await;
    assert!(matches!(&end, WatchEnd::Failed(error) if error.to_string() == "source failed"), "{end:?}");
}

#[tokio::test]
async fn stops_future_delivery_without_aborting_or_awaiting_an_in_flight_callback() {
    let (watch, _) = recorded_watch(json!(0));
    let gate = Gate::new();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (sink, held) = (calls.clone(), gate.clone());
    watch
        .start(move |call, _| {
            let (sink, held) = (sink.clone(), held.clone());
            async move {
                let (value, _, _) = value_call(call)?;
                sink.lock().unwrap().push(format!("start {value}"));
                held.wait().await?;
                sink.lock().unwrap().push(format!("end {value}"));
                Ok(())
            }
        })
        .unwrap();
    watch.advance(frame(json!(1), replace(json!(1))), context());
    flush().await;
    watch.advance(frame(json!(2), replace(json!(2))), context());
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
    gate.resolve();
    flush().await;
    assert_eq!(*calls.lock().unwrap(), ["start 1", "end 1"]);
}

#[tokio::test]
async fn ends_aborted_when_its_acquisition_signal_aborts_also_when_already_aborted() {
    let controller = AbortController::new();
    let (watch, recorded) = recorded_watch(json!(0));
    watch.observe_abort(&controller.signal()).unwrap();
    let repeated = watch.observe_abort(&controller.signal()).unwrap_err();
    assert_eq!(repeated.to_string(), "Watch abort is already installed");
    controller.abort(None);
    let end = watch.closed().await;
    assert!(matches!(end, WatchEnd::Aborted));
    assert_eq!(end.kind(), "aborted");
    assert_eq!(recorded.detached(), 1);

    let (aborted, _) = recorded_watch(json!(0));
    let already = AbortController::new();
    already.abort(None);
    aborted.observe_abort(&already.signal()).unwrap();
    assert!(matches!(aborted.closed().await, WatchEnd::Aborted));

    let (direct, _) = recorded_watch(json!(0));
    direct.abort();
    assert!(matches!(direct.closed().await, WatchEnd::Aborted));
}

#[tokio::test]
async fn ends_listener_error_on_a_throw_reports_it_and_delivers_nothing_more() {
    let (watch, recorded) = recorded_watch(json!(0));
    let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
    let gate = Gate::new();
    let (sink, held) = (seen.clone(), gate.clone());
    watch
        .start(move |call, _| {
            let delivered = value_call(call).map(|(value, _, _)| sink.lock().unwrap().push(value));
            let held = held.clone();
            async move {
                delivered?;
                held.wait().await?;
                Err(ChordError::error("listener failed"))
            }
        })
        .unwrap();
    watch.advance(frame(json!(1), replace(json!(1))), context());
    flush().await;
    watch.advance(frame(json!(2), replace(json!(2))), context());
    gate.resolve();
    let end = watch.closed().await;
    assert!(matches!(&end, WatchEnd::ListenerError(error) if error.to_string() == "listener failed"), "{end:?}");
    assert_eq!(*seen.lock().unwrap(), [json!(1)]);
    assert_eq!(*recorded.reported.lock().unwrap(), ["listener failed"]);
    assert_eq!(recorded.detached(), 1);
}

/// A watch over a source only its closures reference, started and given one frame; `end` ends it.
async fn acquire_watch(end: impl FnOnce(&Watch)) -> (Watch, Weak<()>, Weak<()>) {
    let source = Arc::new(());
    let (in_detach, in_report) = (source.clone(), source.clone());
    let controller = AbortController::new();
    let watch = Watch::new(json!(0), move || drop(in_detach), move |_: &ChordError| drop(in_report.clone()));
    let sentinel = Arc::new(());
    let listener = Arc::downgrade(&sentinel);
    watch.observe_abort(&controller.signal()).unwrap();
    watch
        .start(move |_, _| {
            let _held = sentinel.clone();
            async { Ok(()) }
        })
        .unwrap();
    watch.advance(frame(json!(1), replace(json!(1))), context());
    flush().await;
    end(&watch);
    (watch, Arc::downgrade(&source), listener)
}

#[tokio::test]
async fn retains_neither_its_source_nor_its_listener_once_ended_while_still_referenced() {
    // Control: an open watch keeps its source, so the drops below are real.
    let (open, source, listener) = acquire_watch(|_| {}).await;
    flush().await;
    assert!(source.upgrade().is_some() && listener.upgrade().is_some());
    open.stop().await;

    let ends: [fn(&Watch); 4] = [
        |watch| drop(watch.stop()),
        |watch| watch.abort(),
        |watch| watch.end(WatchEnd::Ended),
        |watch| watch.end_after_delivery(),
    ];
    for end in ends {
        let (_ended, source, listener) = acquire_watch(end).await;
        flush().await;
        assert!(source.upgrade().is_none());
        assert!(listener.upgrade().is_none());
    }
}
