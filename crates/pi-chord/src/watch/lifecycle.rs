//! The lifecycle both watch handles share: pending frames, serialized delivery, and the end of a watch.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::WatchEnd;
use crate::context::{AbortSignal, Context, without_abort_signal};
use crate::delta::Op;
use crate::error::ChordError;
use crate::sync::{BoxFuture, lock};

/// Maximum frames kept behind one unavailable watch listener.
pub(crate) const MAX_PENDING_WATCH_FRAMES: usize = 100;

/// Calls the listener with one frame: its value, its ops, and the delivery context.
pub(crate) type FrameListener<T, E> = Arc<dyn Fn(T, Vec<Op>, Context) -> BoxFuture<Result<(), E>> + Send + Sync>;
pub(crate) type Report<E> = Arc<dyn Fn(&E) + Send + Sync>;
/// Gives the value an overflow delivers instead of the newest one.
pub type Replace<T> = Arc<dyn Fn() -> T + Send + Sync>;

struct Frame<T> {
    value: T,
    ops: Vec<Op>,
    context: Context,
    /// The retirement frame: the watch ends `retired` once it is delivered.
    retires: bool,
}

struct State<T, E> {
    value: T,
    listener: Option<FrameListener<T, E>>,
    pending: VecDeque<Frame<T>>,
    started: bool,
    scheduled: bool,
    running: bool,
    retired: bool,
    /// Set once, by `terminate()`.
    end: Option<WatchEnd<E>>,
    cancellation: Option<(AbortSignal, u64)>,
    /// References into the source, dropped when the watch ends.
    detach: Option<Box<dyn FnOnce() + Send>>,
    report: Option<Report<E>>,
    replace: Option<Replace<T>>,
}

/// A source feeds it and ends it explicitly; the end drops every reference it holds into its source, so a stopped
/// watch that is still referenced keeps nothing but its last value alive.
pub(crate) struct Lifecycle<T, E> {
    state: Mutex<State<T, E>>,
    closed: watch::Sender<Option<WatchEnd<E>>>,
    /// The ops of the frame an overflow delivers instead of the backlog, whose value is the argument.
    overflow_ops: fn(&T) -> Vec<Op>,
}

impl<T, E> Lifecycle<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    pub fn new(
        value: T,
        detach: Box<dyn FnOnce() + Send>,
        report: Report<E>,
        replace: Option<Replace<T>>,
        overflow_ops: fn(&T) -> Vec<Op>,
    ) -> Arc<Self> {
        let state = State {
            value,
            listener: None,
            pending: VecDeque::new(),
            started: false,
            scheduled: false,
            running: false,
            retired: false,
            end: None,
            cancellation: None,
            detach: Some(detach),
            report: Some(report),
            replace,
        };
        Arc::new(Self { state: Mutex::new(state), closed: watch::channel(None).0, overflow_ops })
    }

    pub fn value(&self) -> T {
        lock(&self.state).value.clone()
    }

    pub fn start(self: &Arc<Self>, listener: FrameListener<T, E>) -> Result<(), E> {
        let mut state = lock(&self.state);
        if state.started {
            return Err(ChordError::error("Watch is already started").into());
        }
        if state.end.is_some() {
            return Err(ChordError::error("Watch is stopped").into());
        }
        state.started = true;
        state.listener = Some(listener);
        let schedule = !state.pending.is_empty();
        drop(state);
        if schedule {
            self.schedule();
        }
        Ok(())
    }

    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E> {
        let mut receiver = self.closed.subscribe();
        async move {
            let end = receiver.wait_for(Option::is_some).await.ok().and_then(|end| end.clone());
            end.unwrap_or(WatchEnd::Stopped)
        }
    }

    /// End the watch `cancelled` when `signal` aborts; installed once.
    pub fn observe_cancellation(self: &Arc<Self>, signal: &AbortSignal) -> Result<(), E> {
        let mut state = lock(&self.state);
        if state.cancellation.is_some() {
            return Err(ChordError::error("Watch cancellation is already installed").into());
        }
        if state.end.is_some() {
            return Ok(());
        }
        drop(state);
        let lifecycle = self.clone();
        let id = signal.add_listener(move |_| lifecycle.terminate(WatchEnd::Cancelled));
        state = lock(&self.state);
        if state.end.is_none() {
            state.cancellation = Some((signal.clone(), id));
        } else {
            signal.remove_listener(id);
        }
        drop(state);
        if signal.aborted() {
            self.terminate(WatchEnd::Cancelled);
        }
        Ok(())
    }

    /// Queue one frame; past the bound the backlog collapses to one frame of `replace()` or the newest value.
    pub fn enqueue(self: &Arc<Self>, value: T, ops: Vec<Op>, context: Context) {
        let started = {
            let mut state = lock(&self.state);
            if state.end.is_some() || state.retired {
                return;
            }
            if state.pending.len() >= MAX_PENDING_WATCH_FRAMES {
                state.pending.clear();
                let replacement = state.replace.as_ref().map_or(value, |replace| replace());
                let ops = (self.overflow_ops)(&replacement);
                state.pending.push_back(Frame { value: replacement, ops, context, retires: false });
            } else {
                state.pending.push_back(Frame { value, ops, context, retires: false });
            }
            state.started
        };
        if started {
            self.schedule();
        }
    }

    /// Queue the last frame, after which the watch ends `retired`. An overflowing backlog is dropped, since the frame
    /// replaces the root anyway.
    pub fn enqueue_retirement(self: &Arc<Self>, value: T, ops: Vec<Op>, context: Context) {
        let started = {
            let mut state = lock(&self.state);
            if state.end.is_some() || state.retired {
                return;
            }
            state.retired = true;
            if state.pending.len() >= MAX_PENDING_WATCH_FRAMES {
                state.pending.clear();
            }
            state.pending.push_back(Frame { value, ops, context, retires: true });
            state.started
        };
        if started {
            self.schedule();
        }
    }

    fn schedule(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if state.scheduled || state.running || state.end.is_some() {
                return;
            }
            state.scheduled = true;
        }
        let lifecycle = self.clone();
        tokio::spawn(async move {
            lock(&lifecycle.state).scheduled = false;
            lifecycle.drain().await;
        });
    }

    async fn drain(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if state.running || state.end.is_some() || !state.started {
                return;
            }
            state.running = true;
        }
        loop {
            let (frame, listener) = {
                let mut state = lock(&self.state);
                if state.end.is_some() {
                    break;
                }
                let Some(listener) = state.listener.clone() else { break };
                let Some(frame) = state.pending.pop_front() else { break };
                state.value = frame.value.clone();
                (frame, listener)
            };
            let retires = frame.retires;
            let delivered = listener(frame.value, frame.ops, without_abort_signal(&frame.context)).await;
            drop(listener);
            if let Err(error) = delivered {
                let report = {
                    let state = lock(&self.state);
                    if state.end.is_some() { None } else { state.report.clone() }
                };
                if let Some(report) = report {
                    report(&error);
                    self.terminate(WatchEnd::ListenerError(error));
                }
                break;
            }
            if retires {
                self.terminate(WatchEnd::Retired);
                break;
            }
        }
        let reschedule = {
            let mut state = lock(&self.state);
            state.running = false;
            state.end.is_none() && !state.pending.is_empty()
        };
        if reschedule {
            self.schedule();
        }
    }

    /// End the watch once: detach, drop pending frames and every reference into the source, and settle `closed`. A
    /// callback already running keeps running.
    pub fn terminate(&self, end: WatchEnd<E>) {
        let (cancellation, detach, dropped) = {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(end.clone());
            let pending = std::mem::take(&mut state.pending);
            let dropped = (pending, state.report.take(), state.replace.take(), state.listener.take());
            (state.cancellation.take(), state.detach.take(), dropped)
        };
        drop(dropped);
        if let Some((signal, id)) = cancellation {
            signal.remove_listener(id);
        }
        if let Some(detach) = detach {
            detach();
        }
        self.closed.send_replace(Some(end));
    }
}
