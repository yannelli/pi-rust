//! Failure calls: a failure the source recovers from is a `Failed` call, and the watch stays open.

use std::sync::{Arc, Mutex, Weak};

use pi_chord::{
    ChordError, Context, Json, NativeWatch, NativeWatchCall, WatchCall, WatchEnd, create_context_key, json,
    with_context_value,
};

use super::support::Gate;
use super::*;

fn failed(message: &str) -> Seen {
    Seen::Failed(message.into())
}

fn root(value: Json, retired: bool) -> Seen {
    Seen::Value { value: value.clone(), ops: replace(value), retired }
}

#[tokio::test]
async fn delivers_a_failure_as_a_call_stays_open_keeps_the_last_value_and_replaces_the_root_on_recovery() {
    let (watch, recorded) = recorded_watch(json!({ "n": 0 }));
    let calls = collect_changes(&watch);
    let settled = end_of(&watch);
    watch.advance(frame(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]]))), context());
    flush().await;
    watch.fail(ChordError::error("cannot compute"), context());
    flush().await;
    assert_eq!(seen(&calls)[1], failed("cannot compute"));
    assert_eq!(calls.lock().unwrap()[1].kind(), "failed");
    assert_eq!(watch.value(), json!({ "n": 1 }));
    assert!(settled.lock().unwrap().is_none());
    assert_eq!(recorded.detached(), 0);
    watch.advance(frame(json!({ "n": 2 }), ops(json!([["s", ["n"], 2]]))), context());
    flush().await;
    assert_eq!(seen(&calls)[2], root(json!({ "n": 2 }), false));
    // Recovered: frames pass on as the source made them again.
    let next = frame(json!({ "n": 3 }), ops(json!([["s", ["n"], 3]])));
    watch.advance(next.clone(), context());
    flush().await;
    assert_eq!(seen(&calls)[3], Seen::from(&next));
    assert_eq!(watch.value(), json!({ "n": 3 }));
    watch.stop().await;
}

#[tokio::test]
async fn takes_a_failure_through_advance_as_through_fail() {
    let (watch, _) = recorded_watch(json!(0));
    let calls = collect_changes(&watch);
    watch.advance(WatchCall::Failed { error: ChordError::error("down") }, context());
    flush().await;
    assert_eq!(seen(&calls), [failed("down")]);
    watch.stop().await;
}

#[tokio::test]
async fn folds_pending_values_followed_by_a_failure_into_the_failure() {
    let (watch, _) = recorded_watch(json!({ "n": 0 }));
    watch.advance(frame(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]]))), context());
    watch.advance(frame(json!({ "n": 2 }), ops(json!([["s", ["n"], 2]]))), context());
    watch.fail(ChordError::error("first"), context());
    watch.fail(ChordError::error("second"), context());
    let calls = collect_changes(&watch);
    flush().await;
    assert_eq!(seen(&calls), [failed("second")]);
    assert_eq!(watch.value(), json!({ "n": 0 }));
    watch.stop().await;
}

#[tokio::test]
async fn folds_a_pending_failure_followed_by_values_into_one_root_replacement_with_the_newest_value() {
    let (watch, _) = recorded_watch(json!({ "n": 0 }));
    let key = create_context_key::<String>("frame");
    watch.fail(ChordError::error("down"), with_context_value(&key, "failure".into(), &context()));
    watch.advance(frame(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]]))), context());
    watch.advance(retirement(json!({ "n": 1 })), context());
    let newest = with_context_value(&key, "newest".into(), &context());
    watch.advance(frame(json!({ "m": 2 }), replace(json!({ "m": 2 }))), newest);
    let calls = Arc::new(Mutex::new(Vec::<(Seen, Option<String>)>::new()));
    let (sink, frame_key) = (calls.clone(), key.clone());
    watch
        .start(move |call, delivery| {
            sink.lock().unwrap().push((Seen::from(&call), delivery.value(&frame_key).cloned()));
            async { Ok(()) }
        })
        .unwrap();
    flush().await;
    assert_eq!(*calls.lock().unwrap(), [(root(json!({ "m": 2 }), true), Some("newest".into()))]);
    watch.stop().await;
}

#[tokio::test]
async fn delivers_a_failure_that_arrives_during_a_call_after_it_then_the_recovery() {
    let (watch, _) = recorded_watch(json!(0));
    let gate = Gate::new();
    let calls: Arc<Mutex<Vec<WatchCall>>> = Arc::default();
    let (sink, held) = (calls.clone(), gate.clone());
    watch
        .start(move |call, _| {
            let first = {
                let mut calls = sink.lock().unwrap();
                calls.push(call);
                calls.len() == 1
            };
            let held = held.clone();
            async move { if first { held.wait().await } else { Ok(()) } }
        })
        .unwrap();
    watch.advance(frame(json!(1), replace(json!(1))), context());
    flush().await;
    watch.advance(frame(json!(2), replace(json!(2))), context());
    watch.fail(ChordError::error("down"), context());
    gate.resolve();
    flush().await;
    watch.advance(frame(json!(3), replace(json!(3))), context());
    flush().await;
    assert_eq!(seen(&calls), [Seen::from(&frame(json!(1), replace(json!(1)))), failed("down"), root(json!(3), false)]);
    assert_eq!(watch.value(), json!(3));
    watch.stop().await;
}

#[tokio::test]
async fn drops_a_pending_failure_when_the_source_ends_and_ignores_one_after() {
    let (watch, _) = recorded_watch(json!(0));
    let calls = collect_changes(&watch);
    watch.fail(ChordError::error("down"), context());
    watch.end(WatchEnd::Ended);
    watch.fail(ChordError::error("later"), context());
    flush().await;
    assert!(calls.lock().unwrap().is_empty());
    assert!(matches!(watch.closed().await, WatchEnd::Ended));
}

#[tokio::test]
async fn passes_every_lone_value_call_on_by_identity_so_a_value_call_costs_the_watch_no_allocation() {
    let (watch, _) = recorded_watch(json!(0));
    let calls = collect_changes(&watch);
    let mut sent = Vec::new();
    for n in 1..=1_000u64 {
        let value = json!([n]);
        sent.push(value.clone());
        watch.advance(frame(value.clone(), replace(value)), context());
        while calls.lock().unwrap().len() < n as usize {
            tokio::task::yield_now().await;
        }
    }
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), sent.len());
        for (call, value) in calls.iter().zip(&sent) {
            let WatchCall::Value { value: delivered, ops, .. } = call else { panic!("{call:?}") };
            let [Op::Replace(op)] = ops.as_slice() else { panic!("{ops:?}") };
            assert!(delivered.same_ref(value) && op.same_ref(value));
        }
    }
    watch.stop().await;
}

/// Whether `call` is a failure holding `error` itself (upstream `toBe(error)`): the same `Other` allocation.
fn fails_with(call: Option<&WatchCall>, native: Option<&NativeWatchCall<u32>>, error: &ChordError) -> bool {
    let failure = match (call, native) {
        (Some(WatchCall::Failed { error }), _) | (_, Some(NativeWatchCall::Failed { error })) => error,
        _ => return false,
    };
    match (failure, error) {
        (ChordError::Other(left), ChordError::Other(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

#[tokio::test]
async fn delivers_the_failure_it_was_given_by_identity_through_fail_advance_a_fold_and_a_native_watch() {
    let first = ChordError::other(std::io::Error::other("first"));
    let latest = ChordError::other(std::io::Error::other("latest"));
    let (watch, _) = recorded_watch(json!(0));
    watch.advance(frame(json!(1), replace(json!(1))), context());
    watch.fail(first.clone(), context());
    watch.fail(latest.clone(), context());
    let calls = collect_changes(&watch);
    flush().await;
    let through_advance = ChordError::other(std::io::Error::other("advanced"));
    watch.advance(WatchCall::Failed { error: through_advance.clone() }, context());
    flush().await;
    {
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(fails_with(calls.first(), None, &latest) && !fails_with(calls.first(), None, &first));
        assert!(fails_with(calls.get(1), None, &through_advance));
    }
    assert_eq!(seen(&calls), [failed("latest"), failed("advanced")]);
    watch.stop().await;

    let native = NativeWatch::<u32>::new(0, || {}, |_: &ChordError| {}, None);
    let error = ChordError::other(std::io::Error::other("down"));
    native.advance(1, context());
    native.fail(error.clone(), context());
    let kept: Arc<Mutex<Vec<NativeWatchCall<u32>>>> = Arc::default();
    let sink = kept.clone();
    native
        .start(move |call, _| {
            sink.lock().unwrap().push(call);
            async { Ok(()) }
        })
        .unwrap();
    flush().await;
    let kept = kept.lock().unwrap();
    assert_eq!(kept.len(), 1);
    assert!(fails_with(None, kept.first(), &error));
    assert_eq!(native.value(), 0);
}

/// Calls listeners kept, as they got them.
#[derive(Default)]
struct Kept {
    json: Mutex<Vec<WatchCall>>,
    native: Mutex<Vec<NativeWatchCall<u32>>>,
}

/// Deliver one frame whose context alone references a payload, keep the call, stop the watch, and return the payload.
async fn deliver_keeping_the_call(native: bool, kept: &Arc<Kept>) -> Weak<String> {
    let payload = Arc::new("x".repeat(1000));
    let key = create_context_key::<Arc<String>>("payload");
    let frame_context: Context = with_context_value(&key, payload.clone(), &context());
    let sink = kept.clone();
    if native {
        let watch = NativeWatch::<u32>::new(0, || {}, |_: &ChordError| {}, None);
        watch
            .start(move |call, _| {
                sink.native.lock().unwrap().push(call);
                async { Ok(()) }
            })
            .unwrap();
        watch.advance(1, frame_context);
        flush().await;
        watch.stop().await;
    } else {
        let (watch, _) = recorded_watch(json!(0));
        watch
            .start(move |call, _| {
                sink.json.lock().unwrap().push(call);
                async { Ok(()) }
            })
            .unwrap();
        watch.advance(frame(json!(1), replace(json!(1))), frame_context);
        flush().await;
        watch.stop().await;
    }
    Arc::downgrade(&payload)
}

#[tokio::test]
async fn keeps_no_frame_context_alive_through_a_call_a_listener_keeps() {
    let kept = Arc::new(Kept::default());
    for native in [false, true] {
        let payload = deliver_keeping_the_call(native, &kept).await;
        flush().await;
        let count = kept.json.lock().unwrap().len() + kept.native.lock().unwrap().len();
        assert_eq!(count, if native { 2 } else { 1 });
        assert!(payload.upgrade().is_none());
    }
    // Every field is named, so a native call carries `kind` and `value` only.
    let native = kept.native.lock().unwrap();
    let [call @ NativeWatchCall::Value { value: 1 }] = native.as_slice() else { panic!("{native:?}") };
    assert_eq!(call.kind(), "value");
}
