//! The lifecycle both watch handles share: serialized delivery and the end of a watch. Each handle keeps what is
//! pending in its own store.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::WatchEnd;
use crate::context::{AbortSignal, Context, without_abort_signal};
use crate::error::ChordError;
use crate::sync::{BoxFuture, lock};

/// Calls the listener with one call and the delivery context.
pub(crate) type CallListener<C, E> = Arc<dyn Fn(C, Context) -> BoxFuture<Result<(), E>> + Send + Sync>;
pub(crate) type Report<E> = Arc<dyn Fn(&E) + Send + Sync>;
/// Gives the value an overflow delivers instead of the newest one.
pub type Replace<T> = Arc<dyn Fn() -> T + Send + Sync>;

/// What a handle keeps pending until its listener takes it.
pub(crate) trait PendingStore<C>: Send + 'static {
    /// Take the next call and the context of the newest frame it covers, or `None` when nothing is pending.
    fn next(&mut self) -> Option<(C, Context)>;
    fn has_pending(&self) -> bool;
    /// Drop everything pending and every reference into the source; called once, when the watch ends.
    fn release(&mut self);
}

struct State<T, C, E, P> {
    value: T,
    listener: Option<CallListener<C, E>>,
    pending: P,
    started: bool,
    scheduled: bool,
    running: bool,
    /// Set by `end_after_delivery()`: the source sends nothing more; the watch ends `ended` once it delivered the rest.
    ending: bool,
    /// Set once, by `terminate()`.
    end: Option<WatchEnd<E>>,
    abort: Option<(AbortSignal, u64)>,
    /// References into the source, dropped when the watch ends.
    detach: Option<Box<dyn FnOnce() + Send>>,
    report: Option<Report<E>>,
}

/// A source feeds it and ends it explicitly; the end drops every reference it holds into its source, so a stopped
/// watch that is still referenced keeps nothing but its last value alive.
pub(crate) struct Lifecycle<T, C, E, P> {
    state: Mutex<State<T, C, E, P>>,
    closed: watch::Sender<Option<WatchEnd<E>>>,
    /// The value a call delivers; a `Failed` call has none and keeps the last.
    value_of: fn(&C) -> Option<T>,
}

impl<T, C, E, P> Lifecycle<T, C, E, P>
where
    T: Clone + Send + Sync + 'static,
    C: Send + 'static,
    E: Clone + Send + Sync + From<ChordError> + 'static,
    P: PendingStore<C>,
{
    pub fn new(
        value: T,
        pending: P,
        detach: Box<dyn FnOnce() + Send>,
        report: Report<E>,
        value_of: fn(&C) -> Option<T>,
    ) -> Arc<Self> {
        let state = State {
            value,
            listener: None,
            pending,
            started: false,
            scheduled: false,
            running: false,
            ending: false,
            end: None,
            abort: None,
            detach: Some(detach),
            report: Some(report),
        };
        Arc::new(Self { state: Mutex::new(state), closed: watch::channel(None).0, value_of })
    }

    pub fn value(&self) -> T {
        lock(&self.state).value.clone()
    }

    pub fn start(self: &Arc<Self>, listener: CallListener<C, E>) -> Result<(), E> {
        {
            let mut state = lock(&self.state);
            if state.started {
                return Err(ChordError::error("Watch is already started").into());
            }
            if state.end.is_some() {
                return Err(ChordError::error("Watch is stopped").into());
            }
            state.started = true;
            state.listener = Some(listener);
        }
        self.schedule();
        Ok(())
    }

    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, C, E, P> {
        let mut receiver = self.closed.subscribe();
        async move {
            let end = receiver.wait_for(Option::is_some).await.ok().and_then(|end| end.clone());
            end.unwrap_or(WatchEnd::Stopped)
        }
    }

    /// End the watch `aborted` when `signal` aborts; installed once.
    pub fn observe_abort(self: &Arc<Self>, signal: &AbortSignal) -> Result<(), E> {
        let mut state = lock(&self.state);
        if state.abort.is_some() {
            return Err(ChordError::error("Watch abort is already installed").into());
        }
        if state.end.is_some() {
            return Ok(());
        }
        drop(state);
        let lifecycle = self.clone();
        let id = signal.add_listener(move |_| lifecycle.terminate(WatchEnd::Aborted));
        state = lock(&self.state);
        if state.end.is_none() {
            state.abort = Some((signal.clone(), id));
        } else {
            signal.remove_listener(id);
        }
        drop(state);
        if signal.aborted() {
            self.terminate(WatchEnd::Aborted);
        }
        Ok(())
    }

    /// Change what is pending while the watch takes frames: it has not ended, and its source has not ended. `feed`
    /// says whether something became pending, which is delivered once started, never inline.
    pub fn feed(self: &Arc<Self>, feed: impl FnOnce(&mut P) -> bool) {
        let schedule = {
            let mut state = lock(&self.state);
            if state.end.is_some() || state.ending {
                return;
            }
            feed(&mut state.pending) && state.started
        };
        if schedule {
            self.schedule();
        }
    }

    /// The source sends nothing more after its last frames, such as a retirement: take no more frames, deliver what is
    /// pending, then end `ended`, after the running call returns, if one runs. Until then the watch stays attached, so
    /// its source's end, `stop()`, an abort, and a failing listener still end it first.
    pub fn end_after_delivery(&self) {
        let now = {
            let mut state = lock(&self.state);
            if state.end.is_some() || state.ending {
                return;
            }
            state.ending = true;
            !state.running && !state.pending.has_pending()
        };
        if now {
            self.terminate(WatchEnd::Ended);
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

    /// Deliver until nothing is pending; what arrives during a call is taken after it returns. A watch whose source
    /// ended ends once it delivered the rest and its last call returned.
    async fn drain(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if state.running || state.end.is_some() || !state.started {
                return;
            }
            state.running = true;
        }
        loop {
            let (call, context, listener) = {
                let mut state = lock(&self.state);
                if state.end.is_some() {
                    break;
                }
                let Some(listener) = state.listener.clone() else { break };
                let Some((call, context)) = state.pending.next() else {
                    let ending = state.ending;
                    drop(state);
                    if ending {
                        self.terminate(WatchEnd::Ended);
                    }
                    break;
                };
                if let Some(value) = (self.value_of)(&call) {
                    state.value = value;
                }
                (call, context, listener)
            };
            let delivered = listener(call, without_abort_signal(&context)).await;
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
        }
        // A frame fed after the loop found nothing pending saw the drain running and scheduled nothing.
        let reschedule = {
            let mut state = lock(&self.state);
            state.running = false;
            state.end.is_none() && state.pending.has_pending()
        };
        if reschedule {
            self.schedule();
        }
    }

    /// End the watch once: detach, drop what is pending and every reference into the source, and settle `closed`. A
    /// callback already running keeps running.
    pub fn terminate(&self, end: WatchEnd<E>) {
        let (abort, detach, dropped) = {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(end.clone());
            state.pending.release();
            let dropped = (state.report.take(), state.listener.take());
            (state.abort.take(), state.detach.take(), dropped)
        };
        drop(dropped);
        if let Some((signal, id)) = abort {
            signal.remove_listener(id);
        }
        if let Some(detach) = detach {
            detach();
        }
        self.closed.send_replace(Some(end));
    }
}
