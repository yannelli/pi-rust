//! Execution helpers of the host runtime: task starts that keep JavaScript's order, factory frames, linked
//! abort signals and races against a signal. No lock is held while the code they run is running.

use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use tokio::runtime::Handle;

use super::errors::{ChordErrorCode, ErrorOptions};
use crate::context::{AbortController, AbortSignal};
use crate::error::{ChordError, ChordResult};
use crate::sync::{BoxFuture, lock};

/// Where background errors go (`HostOptions::on_error`).
pub type ErrorHandler = Arc<dyn Fn(ChordError) + Send + Sync>;

/// Polls `future` once on this thread (in the runtime's context), then drives it on the runtime.
/// Like a JavaScript async function, its synchronous start runs before this returns.
pub(crate) fn spawn_now(handle: Option<&Handle>, future: impl Future<Output = ()> + Send + 'static) {
    let mut future: BoxFuture<()> = Box::pin(future);
    match handle.cloned().or_else(|| Handle::try_current().ok()) {
        Some(handle) => {
            let _enter = handle.enter();
            if (&mut future).now_or_never().is_none() {
                handle.spawn(future);
            }
        }
        None => {
            if (&mut future).now_or_never().is_none() {
                std::thread::spawn(move || futures::executor::block_on(future));
            }
        }
    }
}

/// Drives `future` on the runtime from its first poll on: it starts after the caller's synchronous code.
pub(crate) fn spawn_later(handle: Option<&Handle>, future: impl Future<Output = ()> + Send + 'static) {
    match handle.cloned().or_else(|| Handle::try_current().ok()) {
        Some(handle) => {
            handle.spawn(future);
        }
        None => {
            std::thread::spawn(move || futures::executor::block_on(future));
        }
    }
}

struct Frame {
    host: u64,
    misuse: Option<ChordError>,
}

thread_local! {
    static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
}

/// Marks this thread as running a factory of host `host` until dropped (upstream's `inFactory` counter).
pub(crate) struct FactoryFrame;

impl FactoryFrame {
    pub(crate) fn enter(host: u64) -> Self {
        FRAMES.with(|frames| frames.borrow_mut().push(Frame { host, misuse: None }));
        Self
    }

    /// The first misuse recorded while this frame was the innermost one of its host.
    pub(crate) fn take_misuse(&self) -> Option<ChordError> {
        FRAMES.with(|frames| frames.borrow_mut().last_mut().and_then(|frame| frame.misuse.take()))
    }
}

impl Drop for FactoryFrame {
    fn drop(&mut self) {
        FRAMES.with(|frames| frames.borrow_mut().pop());
    }
}

/// Whether this thread runs a factory of host `host` now.
pub(crate) fn in_factory(host: u64) -> bool {
    FRAMES.with(|frames| frames.borrow().iter().any(|frame| frame.host == host))
}

/// Fails the running factory of `host` with `error` once it returns (a call it could not await).
pub(crate) fn record_misuse(host: u64, error: ChordError) {
    FRAMES.with(|frames| {
        if let Some(frame) = frames.borrow_mut().iter_mut().rev().find(|frame| frame.host == host) {
            frame.misuse.get_or_insert(error);
        }
    });
}

/// A signal aborted with the first source to abort, until `release`. Unlike [`AbortSignal::any`], it
/// removes its listeners from the sources, so long-lived sources do not keep every context alive.
pub(crate) struct Linked {
    controller: AbortController,
    detach: Arc<Mutex<Vec<(AbortSignal, u64)>>>,
}

impl Linked {
    pub(crate) fn new(sources: &[AbortSignal]) -> Self {
        let linked = Self { controller: AbortController::new(), detach: Arc::default() };
        for source in sources {
            if let Some(reason) = source.reason() {
                linked.release();
                linked.controller.abort(Some(reason));
                break;
            }
            let (controller, detach) = (linked.controller.clone(), linked.detach.clone());
            let id = source.add_listener(move |reason| {
                release(&detach);
                controller.abort(Some(reason.clone()));
            });
            if id != 0 {
                lock(&linked.detach).push((source.clone(), id));
            }
        }
        linked
    }

    pub(crate) fn signal(&self) -> AbortSignal {
        self.controller.signal()
    }

    pub(crate) fn release(&self) {
        release(&self.detach);
    }
}

fn release(detach: &Mutex<Vec<(AbortSignal, u64)>>) {
    let entries = std::mem::take(&mut *lock(detach));
    for (source, id) in entries {
        source.remove_listener(id);
    }
}

/// `aborted: call aborted`, with the signal's reason as cause.
pub(crate) fn call_aborted(reason: ChordError) -> ChordError {
    ChordError::coded(ChordErrorCode::Aborted, "call aborted", ErrorOptions::default().with_cause(reason))
}

/// `future`, unless `signal` aborts first.
pub(crate) async fn race_signal<T>(
    future: impl Future<Output = ChordResult<T>>,
    signal: &AbortSignal,
) -> ChordResult<T> {
    if let Some(reason) = signal.reason() {
        return Err(call_aborted(reason));
    }
    tokio::select! {
        biased;
        result = future => result,
        reason = signal.cancelled() => Err(call_aborted(reason)),
    }
}

/// Runs a listener; a panic is reported like a thrown error in upstream.
pub(crate) fn guarded(report: &ErrorHandler, listener: impl FnOnce()) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(listener)) {
        report(panic_error(panic.as_ref()));
    }
}

pub(crate) fn panic_error(panic: &(dyn Any + Send)) -> ChordError {
    let message = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into());
    ChordError::error(format!("listener panicked: {message}"))
}
