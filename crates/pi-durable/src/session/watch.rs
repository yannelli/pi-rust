//! Serialized exact-frame watches (spec section 9.2).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use pi_chord::delta::Op;
use pi_chord::{AbortSignal, BoxFuture, Context, Json, without_abort_signal};
use tokio::sync::watch;

use super::WatchEnd;
use super::line::lock;
use super::observation::{CommittedObserver, Release};
use crate::errors::{DurableError, DurableResult};

/// Maximum exact committed frames kept behind one unavailable watch listener.
pub const MAX_PENDING_WATCH_FRAMES: usize = 100;

type Listener = Arc<dyn Fn(Json, Vec<Op>, Context) -> BoxFuture<DurableResult<()>> + Send + Sync>;
type Report = Arc<dyn Fn(&DurableError) + Send + Sync>;
/// Gives the value an overflow delivers.
pub type ReplaceValue = Arc<dyn Fn() -> Json + Send + Sync>;

struct Frame {
    value: Json,
    ops: Vec<Op>,
    context: Context,
}

#[derive(Default)]
struct WatchState {
    value: Json,
    listener: Option<Listener>,
    pending: VecDeque<Frame>,
    started: bool,
    scheduled: bool,
    running: bool,
    retired: bool,
    end: Option<WatchEnd>,
    cancellation: Option<(AbortSignal, u64)>,
}

struct Inner {
    state: Mutex<WatchState>,
    release: Arc<Release>,
    report: Report,
    replace: Option<ReplaceValue>,
    closed: watch::Sender<Option<WatchEnd>>,
}

/// Watch bound to one document incarnation or view (upstream `CommittedWatch` and `WatchHandle`). A `null` value
/// retires it. Cheap to clone; every clone is the same watch.
#[derive(Clone)]
pub struct CommittedWatch(Arc<Inner>);

/// A watch of one document: its value is the document, or `null` once retired.
pub type DocumentWatch = CommittedWatch;

impl CommittedWatch {
    /// `report` gets a listener's error, which also ends the watch. `replace` gives the value an overflow delivers;
    /// by default the newest value.
    pub fn new(
        value: Json,
        release: Arc<Release>,
        report: impl Fn(&DurableError) + Send + Sync + 'static,
        replace: Option<ReplaceValue>,
    ) -> Self {
        let (closed, _) = watch::channel(None);
        let state = WatchState { value, ..WatchState::default() };
        Self(Arc::new(Inner { state: Mutex::new(state), release, report: Arc::new(report), replace, closed }))
    }

    /// The acquisition revision before `start()`, then the latest delivered immutable revision.
    pub fn value(&self) -> Json {
        lock(&self.0.state).value.clone()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> DurableResult<()>
    where
        F: Fn(Json, Vec<Op>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<()>> + Send + 'static,
    {
        let mut state = lock(&self.0.state);
        if state.started {
            return Err(DurableError::error("Watch is already started"));
        }
        if state.end.is_some() {
            return Err(DurableError::error("Watch is stopped"));
        }
        state.started = true;
        state.listener = Some(Arc::new(move |value, ops, context| Box::pin(listener(value, ops, context))));
        let schedule = !state.pending.is_empty();
        drop(state);
        if schedule {
            self.schedule();
        }
        Ok(())
    }

    /// Idempotently stop future callbacks and return the terminal result. A running callback keeps running.
    pub fn stop(&self) -> impl Future<Output = WatchEnd> + Send + 'static {
        self.terminate(WatchEnd::Stopped);
        self.closed()
    }

    /// Settles when the watch terminates.
    pub fn closed(&self) -> impl Future<Output = WatchEnd> + Send + 'static {
        let mut receiver = self.0.closed.subscribe();
        async move {
            let end = receiver.wait_for(Option::is_some).await.ok().and_then(|end| end.clone());
            end.unwrap_or(WatchEnd::Stopped)
        }
    }

    /// End the watch when `signal` aborts.
    pub fn observe_cancellation(&self, signal: &AbortSignal) -> DurableResult<()> {
        let mut state = lock(&self.0.state);
        if state.cancellation.is_some() {
            return Err(DurableError::error("Watch cancellation is already installed"));
        }
        if state.end.is_some() {
            return Ok(());
        }
        drop(state);
        let watch = self.clone();
        let id = signal.add_listener(move |_| watch.cancel());
        state = lock(&self.0.state);
        if state.end.is_none() {
            state.cancellation = Some((signal.clone(), id));
        } else {
            signal.remove_listener(id);
        }
        drop(state);
        if signal.aborted() {
            self.cancel();
        }
        Ok(())
    }

    pub fn cancel(&self) {
        self.terminate(WatchEnd::Cancelled);
    }

    fn schedule(&self) {
        {
            let mut state = lock(&self.0.state);
            if state.scheduled || state.running || state.end.is_some() {
                return;
            }
            state.scheduled = true;
        }
        let watch = self.clone();
        tokio::spawn(async move {
            lock(&watch.0.state).scheduled = false;
            watch.drain().await;
        });
    }

    async fn drain(&self) {
        let listener = {
            let mut state = lock(&self.0.state);
            if state.running || state.end.is_some() || !state.started {
                return;
            }
            let Some(listener) = state.listener.clone() else { return };
            state.running = true;
            listener
        };
        loop {
            let frame = {
                let mut state = lock(&self.0.state);
                if state.end.is_some() {
                    break;
                }
                let Some(frame) = state.pending.pop_front() else { break };
                state.value = frame.value.clone();
                frame
            };
            let retired = frame.value.is_null();
            let delivery = without_abort_signal(&frame.context);
            if let Err(error) = listener(frame.value, frame.ops, delivery).await {
                if lock(&self.0.state).end.is_none() {
                    (self.0.report)(&error);
                    self.terminate(WatchEnd::ListenerError(error));
                }
                break;
            }
            if retired {
                self.terminate(WatchEnd::Retired);
                break;
            }
        }
        let reschedule = {
            let mut state = lock(&self.0.state);
            state.running = false;
            state.end.is_none() && !state.pending.is_empty()
        };
        if reschedule {
            self.schedule();
        }
    }

    /// End the watch once: detach, drop pending frames, and settle `closed`.
    fn terminate(&self, end: WatchEnd) {
        let cancellation = {
            let mut state = lock(&self.0.state);
            if state.end.is_some() {
                return;
            }
            state.end = Some(end.clone());
            state.pending.clear();
            state.cancellation.take()
        };
        self.0.release.run();
        if let Some((signal, id)) = cancellation {
            signal.remove_listener(id);
        }
        self.0.closed.send_replace(Some(end));
    }
}

impl CommittedObserver for CommittedWatch {
    fn advance(&self, value: Json, ops: Vec<Op>, context: Context) {
        let started = {
            let mut state = lock(&self.0.state);
            if state.end.is_some() || state.retired {
                return;
            }
            if value.is_null() {
                state.retired = true;
            }
            if state.pending.len() >= MAX_PENDING_WATCH_FRAMES {
                state.pending.clear();
                let replacement = self.0.replace.as_ref().map_or(value, |replace| replace());
                let ops = vec![Op::Replace(replacement.clone())];
                state.pending.push_back(Frame { value: replacement, ops, context });
            } else {
                state.pending.push_back(Frame { value, ops, context });
            }
            state.started
        };
        if started {
            self.schedule();
        }
    }

    /// End with `session_closed`, or with `session_failed` and the error when the Session failed.
    fn close_session(&self, failure: Option<DurableError>) {
        self.terminate(match failure {
            Some(error) => WatchEnd::SessionFailed(error),
            None => WatchEnd::SessionClosed,
        });
    }

    fn keeps_cancellation(&self) -> bool {
        true
    }
}
