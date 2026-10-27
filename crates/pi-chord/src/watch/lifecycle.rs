//! The lifecycle both watch handles share: serialized delivery and the end of a watch. Each handle keeps what is
//! pending in its own store.

use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

use super::WatchEnd;
use crate::context::{AbortSignal, Context, without_abort_signal};
use crate::delta::Op;
use crate::error::ChordError;
use crate::sync::{BoxFuture, lock};

/// Calls the listener with one delivery: its value, its ops, and the delivery context.
pub(crate) type DeliveryListener<T, E> = Arc<dyn Fn(T, Vec<Op>, Context) -> BoxFuture<Result<(), E>> + Send + Sync>;
pub(crate) type Report<E> = Arc<dyn Fn(&E) + Send + Sync>;
/// Gives the value an overflow delivers instead of the newest one.
pub type Replace<T> = Arc<dyn Fn() -> T + Send + Sync>;

/// One listener call: the value it delivers, its ops, and the context of the newest frame it covers.
pub(crate) struct Delivery<T> {
    pub value: T,
    pub ops: Vec<Op>,
    pub context: Context,
    /// The retirement: the watch ends `retired` once it is delivered.
    pub retires: bool,
}

/// What a handle keeps pending until its listener takes it.
pub(crate) trait PendingStore<T>: Send + 'static {
    /// Take the next delivery, or `None` when nothing is pending.
    fn next(&mut self) -> Option<Delivery<T>>;
    fn has_pending(&self) -> bool;
    /// Drop everything pending and every reference into the source; called once, when the watch ends.
    fn release(&mut self);
}

struct State<T, E, P> {
    value: T,
    listener: Option<DeliveryListener<T, E>>,
    pending: P,
    started: bool,
    scheduled: bool,
    running: bool,
    /// Set once, by `terminate()`.
    end: Option<WatchEnd<E>>,
    cancellation: Option<(AbortSignal, u64)>,
    /// References into the source, dropped when the watch ends.
    detach: Option<Box<dyn FnOnce() + Send>>,
    report: Option<Report<E>>,
}

/// A source feeds it and ends it explicitly; the end drops every reference it holds into its source, so a stopped
/// watch that is still referenced keeps nothing but its last value alive.
pub(crate) struct Lifecycle<T, E, P> {
    state: Mutex<State<T, E, P>>,
    closed: watch::Sender<Option<WatchEnd<E>>>,
}

impl<T, E, P> Lifecycle<T, E, P>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + From<ChordError> + 'static,
    P: PendingStore<T>,
{
    pub fn new(value: T, pending: P, detach: Box<dyn FnOnce() + Send>, report: Report<E>) -> Arc<Self> {
        let state = State {
            value,
            listener: None,
            pending,
            started: false,
            scheduled: false,
            running: false,
            end: None,
            cancellation: None,
            detach: Some(detach),
            report: Some(report),
        };
        Arc::new(Self { state: Mutex::new(state), closed: watch::channel(None).0 })
    }

    pub fn value(&self) -> T {
        lock(&self.state).value.clone()
    }

    pub fn start(self: &Arc<Self>, listener: DeliveryListener<T, E>) -> Result<(), E> {
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

    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E, P> {
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

    /// Change what is pending while the watch takes frames; `feed` says whether something became pending, which is
    /// delivered once started, never inline.
    pub fn feed(self: &Arc<Self>, feed: impl FnOnce(&mut P) -> bool) {
        let schedule = {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            feed(&mut state.pending) && state.started
        };
        if schedule {
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

    /// Deliver until nothing is pending; what arrives during a call is taken after it returns.
    async fn drain(self: &Arc<Self>) {
        {
            let mut state = lock(&self.state);
            if state.running || state.end.is_some() || !state.started {
                return;
            }
            state.running = true;
        }
        loop {
            let (delivery, listener) = {
                let mut state = lock(&self.state);
                if state.end.is_some() {
                    break;
                }
                let Some(listener) = state.listener.clone() else { break };
                let Some(delivery) = state.pending.next() else { break };
                state.value = delivery.value.clone();
                (delivery, listener)
            };
            let retires = delivery.retires;
            let context = without_abort_signal(&delivery.context);
            let delivered = listener(delivery.value, delivery.ops, context).await;
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
        let (cancellation, detach, dropped) = {
            let mut state = lock(&self.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(end.clone());
            state.pending.release();
            let dropped = (state.report.take(), state.listener.take());
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
