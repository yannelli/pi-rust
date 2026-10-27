//! Watch delivery, overflow, and retirement.

use std::sync::{Arc, Mutex};

use pi_chord::{
    AbortController, Context, Json, WatchEnd, create_context_key, json, with_abort_signal, with_context_value,
};

use super::support::Gate;
use super::*;

#[tokio::test]
async fn keeps_the_acquisition_value_until_start_and_delivers_exact_frames_in_order() {
    let (watch, _) = recorded_watch(json!({ "n": 0 }), None);
    watch.advance(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]])), context());
    watch.advance(json!({ "n": 2 }), ops(json!([["s", ["n"], 2]])), context());
    flush().await;
    assert_eq!(watch.value(), json!({ "n": 0 }));

    let frames = collect_frames(&watch);
    assert!(frames.lock().unwrap().is_empty());
    flush().await;
    let expected =
        [(json!({ "n": 1 }), ops(json!([["s", ["n"], 1]]))), (json!({ "n": 2 }), ops(json!([["s", ["n"], 2]])))];
    assert_eq!(*frames.lock().unwrap(), expected);
    assert_eq!(watch.value(), json!({ "n": 2 }));
}

#[tokio::test]
async fn never_invokes_the_listener_inline() {
    let (watch, _, frames) = collecting_watch(json!(0));
    watch.advance(json!(1), replace(json!(1)), context());
    assert!(frames.lock().unwrap().is_empty());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!(1), replace(json!(1)))]);
}

#[tokio::test]
async fn awaits_each_callback_and_buffers_frames_advanced_while_one_is_in_flight() {
    let (watch, _) = recorded_watch(json!(0), None);
    let gate = Gate::new();
    let calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let (sink, held) = (calls.clone(), gate.clone());
    watch
        .start(move |value, _, _| {
            let (sink, held) = (sink.clone(), held.clone());
            async move {
                sink.lock().unwrap().push(format!("start {value}"));
                if value == json!(1) {
                    held.wait().await?;
                }
                sink.lock().unwrap().push(format!("end {value}"));
                Ok(())
            }
        })
        .unwrap();
    watch.advance(json!(1), replace(json!(1)), context());
    flush().await;
    watch.advance(json!(2), replace(json!(2)), context());
    watch.advance(json!(3), replace(json!(3)), context());
    flush().await;
    assert_eq!(*calls.lock().unwrap(), ["start 1"]);
    gate.resolve();
    flush().await;
    assert_eq!(*calls.lock().unwrap(), ["start 1", "end 1", "start 2", "end 2", "start 3", "end 3"]);
}

#[tokio::test]
async fn delivers_a_context_with_the_frame_s_values_and_without_its_abort_signal() {
    let (watch, _) = recorded_watch(json!(0), None);
    let key = create_context_key::<String>("frame");
    let controller = AbortController::new();
    let frame_context = with_context_value(&key, "first".into(), &with_abort_signal(&controller.signal(), &context()));
    let delivered: Arc<Mutex<Option<Context>>> = Arc::default();
    let sink = delivered.clone();
    watch
        .start(move |_, _, delivery| {
            *sink.lock().unwrap() = Some(delivery);
            async { Ok(()) }
        })
        .unwrap();
    watch.advance(json!(1), replace(json!(1)), frame_context);
    flush().await;
    let delivered = delivered.lock().unwrap().clone().unwrap();
    assert_eq!(delivered.value(&key).map(String::as_str), Some("first"));
    assert!(delivered.abort_signal().is_none());
}

#[tokio::test]
async fn collapses_101_pending_frames_to_one_root_replacement_of_the_newest_value() {
    let (watch, _) = recorded_watch(json!(0), None);
    for n in 1..=101 {
        watch.advance(json!(n), replace(json!(n)), context());
    }
    let frames = collect_frames(&watch);
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!(101), replace(json!(101)))]);
}

#[tokio::test]
async fn keeps_100_pending_frames_exact() {
    let (watch, _) = recorded_watch(json!(0), None);
    for n in 1..=100 {
        watch.advance(json!(n), replace(json!(n)), context());
    }
    let frames = collect_frames(&watch);
    flush().await;
    assert_eq!(values(&frames), (1..=100).map(|n| json!(n)).collect::<Vec<_>>());
}

#[tokio::test]
async fn delivers_the_value_of_replace_on_overflow() {
    let (watch, _) = recorded_watch(json!(0), Some(Arc::new(|| json!("replacement"))));
    for n in 1..=101 {
        watch.advance(json!(n), Vec::new(), context());
    }
    let frames = collect_frames(&watch);
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!("replacement"), replace(json!("replacement")))]);
}

#[tokio::test]
async fn never_folds_the_in_flight_frame_into_an_overflow() {
    let (watch, _) = recorded_watch(json!(0), None);
    let gate = Gate::new();
    let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
    let (sink, held) = (seen.clone(), gate.clone());
    watch
        .start(move |value, _, _| {
            let (sink, held) = (sink.clone(), held.clone());
            async move {
                sink.lock().unwrap().push(value.clone());
                if value == json!(1) {
                    held.wait().await?;
                }
                Ok(())
            }
        })
        .unwrap();
    watch.advance(json!(1), replace(json!(1)), context());
    flush().await;
    for n in 2..=102 {
        watch.advance(json!(n), replace(json!(n)), context());
    }
    gate.resolve();
    flush().await;
    assert_eq!(*seen.lock().unwrap(), [json!(1), json!(102)]);
}

#[tokio::test]
async fn delivers_r_null_as_the_last_frame_and_then_ends_retired() {
    let (watch, recorded, frames) = collecting_watch(json!(1));
    let settled = end_of(&watch);
    watch.advance(json!(2), replace(json!(2)), context());
    watch.retire(context());
    watch.advance(json!(3), replace(json!(3)), context());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(json!(2), replace(json!(2))), (Json::Null, replace(Json::Null))]);
    assert_eq!(watch.value(), Json::Null);
    assert!(matches!(*settled.lock().unwrap(), Some(WatchEnd::Retired)));
    assert_eq!(recorded.detached(), 1);
}

#[tokio::test]
async fn drops_an_overflowing_backlog_for_the_retirement_frame() {
    let (watch, _) = recorded_watch(json!(0), None);
    for n in 1..=100 {
        watch.advance(json!(n), replace(json!(n)), context());
    }
    watch.retire(context());
    let frames = collect_frames(&watch);
    assert!(matches!(watch.closed().await, WatchEnd::Retired));
    assert_eq!(*frames.lock().unwrap(), [(Json::Null, replace(Json::Null))]);
}

#[tokio::test]
async fn ends_retired_only_on_its_source_s_retirement_never_on_a_null_value() {
    let (watch, _, frames) = collecting_watch(json!(1));
    let settled = end_of(&watch);
    watch.advance(Json::Null, replace(Json::Null), context());
    watch.advance(json!(2), replace(json!(2)), context());
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(Json::Null, replace(Json::Null)), (json!(2), replace(json!(2)))]);
    assert!(settled.lock().unwrap().is_none());
    assert!(matches!(watch.stop().await, WatchEnd::Stopped));
}

#[tokio::test]
async fn discards_a_retirement_still_pending_when_the_source_ends() {
    let (watch, _) = recorded_watch(json!(1), None);
    watch.retire(context());
    watch.end(None);
    assert!(matches!(watch.closed().await, WatchEnd::Ended));
    assert_eq!(watch.value(), json!(1));
}
