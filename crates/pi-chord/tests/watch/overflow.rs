//! A slow listener, and the overflow of a watch's pending ops.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::delta::{MAX_PENDING_OPS, apply_immutable, track};
use pi_chord::{Json, json};

use super::support::Gate;
use super::*;

#[tokio::test(start_paused = true)]
async fn gets_one_call_per_batch_whose_ops_turn_the_delivered_value_into_the_newest() {
    let mut tracker = track(json!({ "entries": [], "tokens": 0 }));
    let (watch, _) = recorded_watch(tracker.value().clone());
    let replica = Arc::new(Mutex::new(tracker.value().clone()));
    let calls = Arc::new(Mutex::new(Vec::<usize>::new()));
    let (mirror, sink) = (replica.clone(), calls.clone());
    watch
        .start(move |call, _| {
            let (mirror, sink) = (mirror.clone(), sink.clone());
            async move {
                let (value, ops, _) = value_call(call)?;
                let next = apply_immutable(Some(&mirror.lock().unwrap()), &ops).unwrap();
                assert_eq!(next, value);
                *mirror.lock().unwrap() = next;
                sink.lock().unwrap().push(ops.len());
                // A listener taking a second per call, as a slow client would.
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(())
            }
        })
        .unwrap();
    let mut commit = |edit: &dyn Fn(&mut Json)| {
        let mut change = tracker.begin_change();
        edit(change.state().unwrap());
        let prepared = change.prepare().unwrap();
        tracker.adopt(&prepared).unwrap();
        watch.advance(frame(tracker.value().clone(), prepared.ops), context());
    };
    // Ten seconds of a streamed reply at 60 tokens per second, a new entry every second.
    for tick in 0..600 {
        if tick % 60 == 0 {
            commit(&|draft| {
                let entries = draft.get_mut("entries").unwrap().as_array_mut().unwrap();
                entries.push(json!({ "text": "", "status": "streaming" }));
            });
        }
        commit(&|draft| {
            let entries = draft.get_mut("entries").unwrap().as_array_mut().unwrap();
            let last = entries.last_mut().unwrap();
            let text = format!("{}tok ", last["text"].as_str().unwrap());
            last.set("text", text).unwrap();
            let tokens = draft["tokens"].as_i64().unwrap();
            draft.set("tokens", tokens + 1).unwrap();
        });
        tokio::time::sleep(Duration::from_micros(1_000_000 / 60)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(*replica.lock().unwrap(), *tracker.value());
    // One call per second, each carrying at most one op per path written since the call before it.
    let calls = calls.lock().unwrap().clone();
    assert!((10..=12).contains(&calls.len()), "{calls:?}");
    assert!(calls.iter().all(|ops| *ops <= 5), "{calls:?}");
    watch.stop().await;
}

#[tokio::test]
async fn keeps_the_pending_ops_after_folding_and_replaces_the_root_past_them() {
    let (exact, _, frames) = collecting_watch(json!({ "k": [] }));
    advance_distinct(&exact, MAX_PENDING_OPS);
    flush().await;
    assert_eq!(frames.lock().unwrap().len(), 1);
    assert_eq!(frames.lock().unwrap()[0].1.len(), MAX_PENDING_OPS);

    let (over, _, frames) = collecting_watch(json!({ "k": [] }));
    let newest = advance_distinct(&over, MAX_PENDING_OPS + 1);
    flush().await;
    assert_eq!(*frames.lock().unwrap(), [(newest.clone(), replace(newest))]);
}

#[tokio::test]
async fn never_folds_the_in_flight_call_into_an_overflow() {
    let (watch, _) = recorded_watch(json!({ "k": [] }));
    let gate = Gate::new();
    let frames: Frames = Arc::default();
    let (seen, held) = (frames.clone(), gate.clone());
    watch
        .start(move |call, _| {
            let (seen, held) = (seen.clone(), held.clone());
            async move {
                let (value, ops, _) = value_call(call)?;
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push((value, ops));
                    seen.len() == 1
                };
                if first {
                    held.wait().await?;
                }
                Ok(())
            }
        })
        .unwrap();
    watch.advance(frame(json!({ "k": [], "first": true }), ops(json!([["s", ["first"], true]]))), context());
    flush().await;
    let newest = advance_distinct(&watch, MAX_PENDING_OPS + 1);
    gate.resolve();
    flush().await;
    let expected =
        [(json!({ "k": [], "first": true }), ops(json!([["s", ["first"], true]]))), (newest.clone(), replace(newest))];
    assert_eq!(*frames.lock().unwrap(), expected);
}

#[tokio::test]
async fn starts_folding_again_after_an_overflow_was_delivered() {
    let (watch, _, frames) = collecting_watch(json!({ "k": [] }));
    advance_distinct(&watch, MAX_PENDING_OPS + 1);
    flush().await;
    watch.advance(frame(json!({ "n": 1 }), replace(json!({ "n": 1 }))), context());
    watch.advance(frame(json!({ "n": 2 }), ops(json!([["s", ["n"], 2]]))), context());
    flush().await;
    let last = frames.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, (json!({ "n": 2 }), ops(json!([["r", { "n": 1 }], ["s", ["n"], 2]]))));
}
