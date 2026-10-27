//! `NativeWatch`: in-process values without ops.

use std::sync::{Arc, Mutex, Weak};

use pi_chord::{
    AbortController, ChordError, NativeWatch, NativeWatchChange, WatchEnd, create_context_key, with_context_value,
};

use super::*;

#[tokio::test]
async fn delivers_values_in_order_with_a_context_never_inline() {
    let watch = NativeWatch::<Arc<str>>::new("zero".into(), || {}, |_: &ChordError| {}, None);
    let key = create_context_key::<String>("frame");
    let delivered = Arc::new(Mutex::new(Vec::<(Arc<str>, Option<String>)>::new()));
    let (sink, frame_key) = (delivered.clone(), key.clone());
    watch
        .start(move |NativeWatchChange { value }, delivery| {
            sink.lock().unwrap().push((value, delivery.value(&frame_key).cloned()));
            async { Ok(()) }
        })
        .unwrap();
    let (first, second): (Arc<str>, Arc<str>) = ("first".into(), "second".into());
    watch.advance(first.clone(), with_context_value(&key, "first".into(), &context()));
    watch.advance(second.clone(), with_context_value(&key, "second".into(), &context()));
    assert!(delivered.lock().unwrap().is_empty());
    flush().await;
    let delivered = delivered.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    assert!(Arc::ptr_eq(&delivered[0].0, &first) && Arc::ptr_eq(&delivered[1].0, &second));
    assert_eq!(delivered.iter().map(|entry| entry.1.as_deref()).collect::<Vec<_>>(), [Some("first"), Some("second")]);
    assert!(Arc::ptr_eq(&watch.value(), &second));
}

#[tokio::test]
async fn collapses_101_pending_values_to_the_newest() {
    let watch = NativeWatch::<u32>::new(0, || {}, |_: &ChordError| {}, None);
    for n in 1..=101 {
        watch.advance(n, context());
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    watch
        .start(move |NativeWatchChange { value }, _| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        })
        .unwrap();
    flush().await;
    assert_eq!(*seen.lock().unwrap(), [101]);
}

#[tokio::test]
async fn collapses_101_pending_values_to_the_value_of_replace() {
    let replace: pi_chord::watch::Replace<String> = Arc::new(|| "replacement".into());
    let watch = NativeWatch::<String>::new(String::new(), || {}, |_: &ChordError| {}, Some(replace));
    for n in 1..=101 {
        watch.advance(format!("value {n}"), context());
    }
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    watch
        .start(move |NativeWatchChange { value }, _| {
            sink.lock().unwrap().push(value);
            async { Ok(()) }
        })
        .unwrap();
    flush().await;
    assert_eq!(*seen.lock().unwrap(), ["replacement"]);
}

#[tokio::test]
async fn ends_with_every_reason_of_its_lifecycle() {
    let create = || NativeWatch::<u32>::new(0, || {}, |_: &ChordError| {}, None);
    assert!(matches!(create().stop().await, WatchEnd::Stopped));

    let cancelled = create();
    let controller = AbortController::new();
    controller.abort(None);
    cancelled.observe_cancellation(&controller.signal()).unwrap();
    assert!(matches!(cancelled.closed().await, WatchEnd::Cancelled));

    let ended = create();
    ended.end(None);
    assert!(matches!(ended.closed().await, WatchEnd::Ended));

    let failed = create();
    failed.end(Some(ChordError::error("source failed")));
    assert!(matches!(failed.closed().await, WatchEnd::Failed(error) if error.to_string() == "source failed"));

    let reported = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = reported.clone();
    let report = move |error: &ChordError| sink.lock().unwrap().push(error.to_string());
    let throwing = NativeWatch::<u32>::new(0, || {}, report, None);
    throwing.start(|_, _| async { Err(ChordError::error("listener failed")) }).unwrap();
    throwing.advance(1, context());
    let end = throwing.closed().await;
    assert!(matches!(&end, WatchEnd::ListenerError(error) if error.to_string() == "listener failed"), "{end:?}");
    assert_eq!(*reported.lock().unwrap(), ["listener failed"]);
}

#[tokio::test]
async fn retains_neither_its_source_nor_its_listener_once_stopped_while_still_referenced() {
    async fn acquire(stop: bool) -> (NativeWatch<u32>, Weak<()>, Weak<()>) {
        let source = Arc::new(());
        let (in_detach, in_report) = (source.clone(), source.clone());
        let report = move |_: &ChordError| drop(in_report.clone());
        let watch = NativeWatch::new(0, move || drop(in_detach), report, None);
        let sentinel = Arc::new(());
        let listener = Arc::downgrade(&sentinel);
        watch
            .start(move |_, _| {
                let _held = sentinel.clone();
                async { Ok(()) }
            })
            .unwrap();
        watch.advance(1, context());
        flush().await;
        if stop {
            watch.stop().await;
        }
        (watch, Arc::downgrade(&source), listener)
    }

    let (open, source, _) = acquire(false).await;
    assert!(source.upgrade().is_some());
    open.stop().await;

    let (stopped, source, listener) = acquire(true).await;
    assert!(source.upgrade().is_none());
    assert!(listener.upgrade().is_none());
    assert_eq!(stopped.value(), 1);
}
