//! Watch delivery and retirement.

use std::sync::{Arc, Mutex};

use pi_chord::{
    AbortController, Context, Json, WatchCall, WatchEnd, create_context_key, json, with_abort_signal,
    with_context_value,
};

use super::support::Gate;
use super::*;

#[tokio::test]
async fn keeps_the_acquisition_value_until_start_then_delivers_the_frames_since_in_one_call() {
    let (watch, _) = recorded_watch(json!({ "n": 0, "text": "" }));
    watch.advance(frame(json!({ "n": 1, "text": "" }), ops(json!([["s", ["n"], 1]]))), context());
    watch
        .advance(frame(json!({ "n": 2, "text": "a" }), ops(json!([["s", ["n"], 2], ["a", ["text"], "a"]]))), context());
    watch.advance(frame(json!({ "n": 2, "text": "ab" }), ops(json!([["a", ["text"], "b"]]))), context());
    flush().await;
    assert_eq!(watch.value(), json!({ "n": 0, "text": "" }));

    let frames = collect_frames(&watch);
    assert!(frames.lock().unwrap().is_empty());
    flush().await;
    let expected = (json!({ "n": 2, "text": "ab" }), ops(json!([["s", ["n"], 2], ["a", ["text"], "ab"]])));
    assert_eq!(*frames.lock().unwrap(), [expected]);
    assert_eq!(watch.value(), json!({ "n": 2, "text": "ab" }));
}

#[tokio::test]
async fn passes_a_frame_s_ops_on_unchanged_to_a_listener_that_keeps_up() {
    let (watch, _, frames) = collecting_watch(json!(0));
    watch.advance(frame(json!(1), replace(json!(1))), context());
    flush().await;
    watch.advance(frame(json!(2), replace(json!(2))), context());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!(1), replace(json!(1))), (json!(2), replace(json!(2)))]);
}

#[tokio::test]
async fn never_invokes_the_listener_inline() {
    let (watch, _, frames) = collecting_watch(json!(0));
    watch.advance(frame(json!(1), replace(json!(1))), context());
    assert!(frames.lock().unwrap().is_empty());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!(1), replace(json!(1)))]);
}

#[tokio::test]
async fn awaits_each_callback_and_folds_frames_advanced_while_one_is_in_flight_into_the_next_call() {
    let (watch, _) = recorded_watch(json!({ "text": "" }));
    let gate = Gate::new();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let frames: Frames = Arc::default();
    let (sink, held, seen) = (calls.clone(), gate.clone(), frames.clone());
    watch
        .start(move |call, _| {
            let (sink, held, seen) = (sink.clone(), held.clone(), seen.clone());
            async move {
                let (value, ops, _) = value_call(call)?;
                sink.lock().unwrap().push(format!("start {value}"));
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push((value.clone(), ops));
                    seen.len() == 1
                };
                if first {
                    held.wait().await?;
                }
                sink.lock().unwrap().push(format!("end {value}"));
                Ok(())
            }
        })
        .unwrap();
    watch.advance(frame(json!({ "text": "a" }), ops(json!([["a", ["text"], "a"]]))), context());
    flush().await;
    watch.advance(frame(json!({ "text": "ab" }), ops(json!([["a", ["text"], "b"]]))), context());
    watch.advance(frame(json!({ "text": "abc" }), ops(json!([["a", ["text"], "c"]]))), context());
    flush().await;
    assert_eq!(*calls.lock().unwrap(), [r#"start {"text":"a"}"#]);
    gate.resolve();
    flush().await;
    let expected = [r#"start {"text":"a"}"#, r#"end {"text":"a"}"#, r#"start {"text":"abc"}"#, r#"end {"text":"abc"}"#];
    assert_eq!(*calls.lock().unwrap(), expected);
    assert_eq!(frames.lock().unwrap()[1], (json!({ "text": "abc" }), ops(json!([["a", ["text"], "bc"]]))));
}

#[tokio::test]
async fn delivers_the_newest_frame_s_context_without_its_abort_signal() {
    let (watch, _) = recorded_watch(json!(0));
    let key = create_context_key::<String>("frame");
    let controller = AbortController::new();
    let delivered: Arc<Mutex<Vec<Context>>> = Arc::default();
    let sink = delivered.clone();
    watch
        .start(move |_, delivery| {
            sink.lock().unwrap().push(delivery);
            async { Ok(()) }
        })
        .unwrap();
    watch.advance(frame(json!(1), replace(json!(1))), with_context_value(&key, "first".into(), &context()));
    let second = with_context_value(&key, "second".into(), &with_abort_signal(&controller.signal(), &context()));
    watch.advance(frame(json!(2), replace(json!(2))), second);
    flush().await;
    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].value(&key).map(String::as_str), Some("second"));
    assert!(delivered[0].abort_signal().is_none());
}

#[tokio::test]
async fn reports_a_retirement_in_the_next_call_with_the_value_the_source_gives_and_goes_on() {
    let (watch, recorded) = recorded_watch(json!(1));
    let changes = collect_changes(&watch);
    let settled = end_of(&watch);
    watch.advance(frame(json!(2), replace(json!(2))), context());
    flush().await;
    watch.advance(retirement(json!(2)), context());
    flush().await;
    watch.advance(frame(json!(3), replace(json!(3))), context());
    flush().await;
    let expected = [frame(json!(2), replace(json!(2))), retirement(json!(2)), frame(json!(3), replace(json!(3)))];
    assert_eq!(seen(&changes), expected.iter().map(Seen::from).collect::<Vec<_>>());
    assert_eq!(watch.value(), json!(3));
    assert!(settled.lock().unwrap().is_none());
    assert_eq!(recorded.detached(), 0);
    watch.stop().await;
}

#[tokio::test]
async fn folds_a_retirement_and_the_frames_around_it_into_one_call_that_reports_it() {
    let (watch, _) = recorded_watch(json!({ "n": 0 }));
    watch.advance(frame(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]]))), context());
    watch.advance(retirement(json!({ "n": 1 })), context());
    watch.advance(frame(json!({ "m": 1 }), replace(json!({ "m": 1 }))), context());
    let changes = collect_changes(&watch);
    flush().await;
    let folded = WatchCall::Value { value: json!({ "m": 1 }), ops: replace(json!({ "m": 1 })), retired: true };
    assert_eq!(seen(&changes), [Seen::from(&folded)]);
    watch.advance(frame(json!({ "m": 2 }), ops(json!([["s", ["m"], 2]]))), context());
    flush().await;
    assert_eq!(seen(&changes)[1], Seen::from(&frame(json!({ "m": 2 }), ops(json!([["s", ["m"], 2]])))));
    watch.stop().await;
}

#[tokio::test]
async fn keeps_reporting_the_retirement_past_an_overflow() {
    let (watch, _) = recorded_watch(json!({ "k": [] }));
    watch.advance(retirement(json!({ "k": [] })), context());
    let newest = advance_distinct(&watch, pi_chord::delta::MAX_PENDING_OPS + 1);
    let changes = collect_changes(&watch);
    flush().await;
    let expected = WatchCall::Value { value: newest.clone(), ops: replace(newest), retired: true };
    assert_eq!(seen(&changes), [Seen::from(&expected)]);
    watch.stop().await;
}

#[tokio::test]
async fn treats_a_null_value_as_a_value_never_as_a_retirement() {
    let (watch, _, frames) = collecting_watch(json!(1));
    let settled = end_of(&watch);
    watch.advance(frame(Json::Null, replace(Json::Null)), context());
    flush().await;
    watch.advance(frame(json!(2), replace(json!(2))), context());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(Json::Null, replace(Json::Null)), (json!(2), replace(json!(2)))]);
    assert!(settled.lock().unwrap().is_none());
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
}

#[tokio::test]
async fn discards_a_retirement_still_pending_when_the_source_ends() {
    let (watch, _) = recorded_watch(json!(1));
    watch.advance(retirement(json!(1)), context());
    watch.end(WatchEnd::Ended);
    assert!(matches!(watch.closed().await, WatchEnd::Ended));
    assert_eq!(watch.value(), json!(1));
}
