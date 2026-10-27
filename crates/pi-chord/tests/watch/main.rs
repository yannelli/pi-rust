//! Port of upstream `test/watch.test.ts`: the shared lifecycle through `Watch`, and what `NativeWatch` does
//! differently.

#[path = "../support/mod.rs"]
mod support;

mod delivery;
mod ends;
mod native;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::delta::{Op, parse_ops};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, Context, Json, Watch, WatchEnd};

pub fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

pub async fn flush() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

pub fn ops(batch: Json) -> Vec<Op> {
    parse_ops(&batch).unwrap()
}

pub fn replace(value: Json) -> Vec<Op> {
    vec![Op::Replace(value)]
}

pub type Frames = Arc<Mutex<Vec<(Json, Vec<Op>)>>>;

#[derive(Clone, Default)]
pub struct Recorded {
    pub detached: Arc<AtomicUsize>,
    pub reported: Arc<Mutex<Vec<String>>>,
}

impl Recorded {
    pub fn detached(&self) -> usize {
        self.detached.load(Ordering::SeqCst)
    }
}

/// A watch whose detach and reports are recorded.
pub fn recorded_watch(value: Json, replace: Option<Arc<dyn Fn() -> Json + Send + Sync>>) -> (Watch, Recorded) {
    let recorded = Recorded::default();
    let (detached, reported) = (recorded.detached.clone(), recorded.reported.clone());
    let watch = Watch::new(
        value,
        move || {
            detached.fetch_add(1, Ordering::SeqCst);
        },
        move |error: &ChordError| reported.lock().unwrap().push(error.to_string()),
        replace,
    );
    (watch, recorded)
}

pub fn collect_frames(watch: &Watch) -> Frames {
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(move |value, ops, _| {
            sink.lock().unwrap().push((value, ops));
            async { Ok(()) }
        })
        .unwrap();
    frames
}

/// A started watch collecting its frames.
pub fn collecting_watch(value: Json) -> (Watch, Recorded, Frames) {
    let (watch, recorded) = recorded_watch(value, None);
    let frames = collect_frames(&watch);
    (watch, recorded, frames)
}

/// The end of `closed`, or `None` while the watch is open.
pub fn end_of(watch: &Watch) -> Arc<Mutex<Option<WatchEnd>>> {
    let settled: Arc<Mutex<Option<WatchEnd>>> = Arc::default();
    let (sink, closed) = (settled.clone(), watch.closed());
    tokio::spawn(async move { *sink.lock().unwrap() = Some(closed.await) });
    settled
}

pub fn values(frames: &Frames) -> Vec<Json> {
    frames.lock().unwrap().iter().map(|(value, _)| value.clone()).collect()
}
