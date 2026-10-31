//! `end_after_delivery()`: the source sends nothing more, and the watch ends `ended` once it delivered the rest.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pi_chord::{ChordError, Json, NativeWatch, Watch, WatchCall, WatchEnd, json};

use super::support::Gate;
use super::*;

#[tokio::test]
async fn takes_no_more_frames_delivers_what_is_pending_then_ends_ended_and_only_then_detaches() {
    let (watch, recorded) = recorded_watch(json!(1));
    let changes = collect_changes(&watch);
    let settled = end_of(&watch);
    watch.advance(retirement(json!(1)), context());
    watch.end_after_delivery();
    assert_eq!(recorded.detached(), 0);
    // The source sends nothing more: a later frame is not taken.
    watch.advance(frame(json!(2), replace(json!(2))), context());
    flush().await;
    assert_eq!(seen(&changes), [Seen::from(&retirement(json!(1)))]);
    assert!(matches!(*settled.lock().unwrap(), Some(WatchEnd::Ended)));
    assert_eq!(recorded.detached(), 1);
}

#[tokio::test]
async fn waits_for_start_to_deliver_and_ends_at_once_with_nothing_pending() {
    let (watch, recorded) = recorded_watch(json!(1));
    watch.advance(frame(json!(2), replace(json!(2))), context());
    watch.end_after_delivery();
    flush().await;
    let settled = end_of(&watch);
    flush().await;
    assert!(settled.lock().unwrap().is_none());
    assert_eq!(recorded.detached(), 0);
    let frames = collect_frames(&watch);
    assert!(matches!(watch.closed().await, WatchEnd::Ended));
    assert_eq!(values(&frames), [json!(2)]);
    assert_eq!(recorded.detached(), 1);

    let (idle, recorded, frames) = collecting_watch(json!(1));
    idle.end_after_delivery();
    assert!(matches!(idle.closed().await, WatchEnd::Ended));
    assert!(frames.lock().unwrap().is_empty());
    assert_eq!(recorded.detached(), 1);
}

#[tokio::test]
async fn ends_a_never_started_watch_at_its_source_s_end_dropping_what_is_pending() {
    let (watch, recorded) = recorded_watch(json!(1));
    watch.advance(retirement(json!(1)), context());
    watch.end_after_delivery();
    watch.end(WatchEnd::Failed(ChordError::error("source failed")));
    let end = watch.closed().await;
    assert!(matches!(&end, WatchEnd::Failed(error) if error.to_string() == "source failed"), "{end:?}");
    assert_eq!(recorded.detached(), 1);
    assert_eq!(watch.start(|_, _| async { Ok(()) }).unwrap_err().to_string(), "Watch is stopped");
}

struct Ending {
    watch: Watch,
    recorded: Recorded,
    values: Arc<Mutex<Vec<Json>>>,
    settled: Arc<Mutex<Option<WatchEnd>>>,
    gate: Gate,
    returned: Arc<AtomicBool>,
}

/// A started watch whose listener is in a call held by a gate, with nothing pending, then `end_after_delivery()`; the
/// call then fails when `fails` says so.
async fn ending_while_running(fails: bool) -> Ending {
    let (watch, recorded) = recorded_watch(json!(0));
    let (gate, values, returned) = (Gate::new(), Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicBool::new(false)));
    let (held, sink, done) = (gate.clone(), values.clone(), returned.clone());
    watch
        .start(move |call, _| {
            let delivered = value_call(call).map(|(value, _, _)| sink.lock().unwrap().push(value));
            let (held, done) = (held.clone(), done.clone());
            async move {
                delivered?;
                held.wait().await?;
                if fails {
                    return Err(ChordError::error("listener failed"));
                }
                done.store(true, Ordering::SeqCst);
                Ok(())
            }
        })
        .unwrap();
    watch.advance(WatchCall::Value { value: json!(1), ops: replace(json!(1)), retired: true }, context());
    flush().await;
    assert_eq!(*values.lock().unwrap(), [json!(1)]);
    watch.end_after_delivery();
    let settled = end_of(&watch);
    flush().await;
    Ending { watch, recorded, values, settled, gate, returned }
}

#[tokio::test]
async fn ends_ended_only_after_the_running_call_returns_when_nothing_is_pending_detached_then() {
    let ending = ending_while_running(false).await;
    assert!(ending.settled.lock().unwrap().is_none());
    assert_eq!(ending.recorded.detached(), 0);
    ending.gate.resolve();
    assert!(matches!(ending.watch.closed().await, WatchEnd::Ended));
    assert!(ending.returned.load(Ordering::SeqCst));
    assert_eq!(*ending.values.lock().unwrap(), [json!(1)]);
    assert_eq!(ending.recorded.detached(), 1);
}

#[tokio::test]
async fn ends_listener_error_and_reports_it_when_the_running_call_fails_after_end_after_delivery() {
    let ending = ending_while_running(true).await;
    assert!(ending.settled.lock().unwrap().is_none());
    ending.gate.resolve();
    let end = ending.watch.closed().await;
    assert!(matches!(&end, WatchEnd::ListenerError(error) if error.to_string() == "listener failed"), "{end:?}");
    assert_eq!(*ending.recorded.reported.lock().unwrap(), ["listener failed"]);
    assert_eq!(ending.recorded.detached(), 1);
}

#[tokio::test]
async fn ends_stopped_when_stopped_while_ending_without_awaiting_the_running_call() {
    let ending = ending_while_running(false).await;
    assert!(matches!(ending.watch.stop().await, WatchEnd::Stopped));
    assert_eq!(ending.recorded.detached(), 1);
    assert!(!ending.returned.load(Ordering::SeqCst));
    ending.gate.resolve();
    flush().await;
    assert!(matches!(ending.watch.closed().await, WatchEnd::Stopped));
}

/// Upstream "hands a listener plain data only: `{ kind, value, ops, retired }`" and "passes a frame delivered alone on
/// as the source made it".
#[tokio::test]
async fn hands_a_listener_plain_data_only_kind_value_ops_retired_and_passes_a_frame_delivered_alone_on() {
    let (watch, _) = recorded_watch(json!({ "n": 0 }));
    let changes = collect_changes(&watch);
    let alone = frame(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]])));
    watch.advance(alone.clone(), context());
    flush().await;
    // Folded: the watch builds the change.
    watch.advance(frame(json!({ "n": 2 }), ops(json!([["s", ["n"], 2]]))), context());
    watch.advance(retirement(json!({ "n": 2 })), context());
    flush().await;
    let calls = changes.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        // Every field is named, so a call carries `kind`, `value`, `ops` and `retired` only.
        let WatchCall::Value { value: _, ops: _, retired: _ } = call else { panic!("{call:?}") };
        assert_eq!(call.kind(), "value");
        assert_eq!(Seen::from(&call.clone()), Seen::from(call));
    }
    let folded = WatchCall::Value { value: json!({ "n": 2 }), ops: ops(json!([["s", ["n"], 2]])), retired: true };
    assert_eq!(seen(&changes), [Seen::from(&alone), Seen::from(&folded)]);
    watch.stop().await;
}

#[tokio::test]
async fn delivers_a_native_backlog_that_never_drains_in_order() {
    let watch = NativeWatch::<u32>::new(0, || {}, |_: &ChordError| {}, None);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (sink, feeder) = (seen.clone(), watch.clone());
    let next = Arc::new(Mutex::new(50u32));
    // Each call queues one more value behind a backlog of 50, so the queue never empties for 1,000 values.
    watch
        .start(move |call, _| {
            let Ok(value) = native_value_call(call) else { return std::future::ready(Ok(())) };
            sink.lock().unwrap().push(value);
            let mut next = next.lock().unwrap();
            if *next < 1_000 {
                feeder.advance(*next, context());
                *next += 1;
            }
            std::future::ready(Ok(()))
        })
        .unwrap();
    for n in 0..50 {
        watch.advance(n, context());
    }
    pi_chord_wait(|| seen.lock().unwrap().len() == 1_000).await;
    assert_eq!(*seen.lock().unwrap(), (0..1_000).collect::<Vec<_>>());
    watch.stop().await;
}

async fn pi_chord_wait(mut condition: impl FnMut() -> bool) {
    super::support::wait_for(&mut condition).await;
}
