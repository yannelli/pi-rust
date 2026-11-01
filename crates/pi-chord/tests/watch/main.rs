//! Port of upstream `test/watch.test.ts`: the shared lifecycle through `Watch`, and what `NativeWatch` does
//! differently.

#[path = "../support/mod.rs"]
mod support;

mod delivery;
mod ending;
mod ends;
mod failures;
mod native;
mod overflow;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pi_chord::delta::{Op, Seg, parse_ops};
use pi_chord::{BACKGROUND_CONTEXT, ChordError, Context, Json, NativeWatchCall, Watch, WatchCall, WatchEnd};

pub fn context() -> Context {
    BACKGROUND_CONTEXT.clone()
}

pub async fn flush() {
    tokio::time::sleep(Duration::from_millis(5)).await;
}

pub fn ops(batch: Json) -> Vec<Op> {
    parse_ops(&batch).unwrap()
}

/// A source's frame that is not a retirement.
pub fn frame(value: Json, ops: Vec<Op>) -> WatchCall {
    WatchCall::new(value, ops)
}

/// A source's retirement frame.
pub fn retirement(value: Json) -> WatchCall {
    WatchCall::Value { value, ops: Vec::new(), retired: true }
}

/// The fields of a value call (upstream `onValue`): a `Failed` call is an error, which ends the watch.
pub fn value_call(call: WatchCall) -> Result<(Json, Vec<Op>, bool), ChordError> {
    match call {
        WatchCall::Value { value, ops, retired } => Ok((value, ops, retired)),
        WatchCall::Failed { .. } => Err(ChordError::error("Unexpected failed call")),
    }
}

/// A call as comparable data, a failure by its message (`ChordError` has no equality).
#[derive(Debug, PartialEq)]
pub enum Seen {
    Value { value: Json, ops: Vec<Op>, retired: bool },
    Failed(String),
}

impl From<&WatchCall> for Seen {
    fn from(call: &WatchCall) -> Self {
        match call {
            WatchCall::Value { value, ops, retired } => {
                Self::Value { value: value.clone(), ops: ops.clone(), retired: *retired }
            }
            WatchCall::Failed { error } => Self::Failed(error.to_string()),
        }
    }
}

/// Every call kept so far, as comparable data.
pub fn seen(calls: &Mutex<Vec<WatchCall>>) -> Vec<Seen> {
    calls.lock().unwrap().iter().map(Seen::from).collect()
}

/// The value of a native value call, as `value_call`.
pub fn native_value_call<T>(call: NativeWatchCall<T>) -> Result<T, ChordError> {
    match call {
        NativeWatchCall::Value { value } => Ok(value),
        NativeWatchCall::Failed { .. } => Err(ChordError::error("Unexpected failed call")),
    }
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
pub fn recorded_watch(value: Json) -> (Watch, Recorded) {
    let recorded = Recorded::default();
    let (detached, reported) = (recorded.detached.clone(), recorded.reported.clone());
    let watch = Watch::new(
        value,
        move || {
            detached.fetch_add(1, Ordering::SeqCst);
        },
        move |error: &ChordError| reported.lock().unwrap().push(error.to_string()),
    );
    (watch, recorded)
}

pub fn collect_frames(watch: &Watch) -> Frames {
    let frames: Frames = Arc::default();
    let sink = frames.clone();
    watch
        .start(move |call, _| {
            let delivered = value_call(call).map(|(value, ops, _)| sink.lock().unwrap().push((value, ops)));
            async move { delivered }
        })
        .unwrap();
    frames
}

/// A started watch collecting its calls.
pub fn collecting_watch(value: Json) -> (Watch, Recorded, Frames) {
    let (watch, recorded) = recorded_watch(value);
    let frames = collect_frames(&watch);
    (watch, recorded, frames)
}

/// Start `watch` with a listener that keeps every call it gets, failures included.
pub fn collect_changes(watch: &Watch) -> Arc<Mutex<Vec<WatchCall>>> {
    let changes: Arc<Mutex<Vec<WatchCall>>> = Arc::default();
    let sink = changes.clone();
    watch
        .start(move |change, _| {
            sink.lock().unwrap().push(change);
            async { Ok(()) }
        })
        .unwrap();
    changes
}

/// The end of `closed`, or `None` while the watch is open.
pub fn end_of(watch: &Watch) -> Arc<Mutex<Option<WatchEnd>>> {
    let settled: Arc<Mutex<Option<WatchEnd>>> = Arc::default();
    let (sink, closed) = (settled.clone(), watch.closed());
    tokio::spawn(async move { *sink.lock().unwrap() = Some(closed.await) });
    settled
}

/// Advance `watch` by one frame per element of `{ k: [0, ..., count - 1] }`, each setting its own path. Only the
/// newest value is ever delivered, so the frames before it carry a placeholder instead of their own array.
pub fn advance_distinct(watch: &Watch, count: usize) -> Json {
    let newest = Json::from(serde_json::json!({ "k": (0..count as u64).collect::<Vec<_>>() }));
    for index in 0..count {
        let value = if index + 1 == count { newest.clone() } else { Json::Null };
        let path = vec![Seg::key("k"), Seg::Index(index as u64)];
        watch.advance(frame(value, vec![Op::Set(path, Json::from(index as u64))]), context());
    }
    newest
}

pub fn values(frames: &Frames) -> Vec<Json> {
    frames.lock().unwrap().iter().map(|(value, _)| value.clone()).collect()
}
