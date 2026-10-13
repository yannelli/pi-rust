//! Public state listeners and the per-subscription delivery queue.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::FutureExt;

use crate::context::Context;
use crate::error::{ChordError, ChordResult, report_unhandled};
use crate::json::Json;
use crate::sync::{BoxFuture, lock, spawn_eager};

/// Whether a delivery installs a fresh value or follows the one before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    Hydrate,
    Update,
}

/// Metadata for one complete-value delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplicatedStateDelivery {
    pub kind: DeliveryKind,
    pub sequence: u64,
}

pub(crate) enum ListenerOutcome {
    Done(ChordResult<()>),
    Pending(BoxFuture<ChordResult<()>>),
}

type ListenerFn = dyn Fn(Json, Context, ReplicatedStateDelivery) -> ListenerOutcome + Send + Sync;

/// A state subscription callback, synchronous or asynchronous.
#[derive(Clone)]
pub struct StateListener(Arc<ListenerFn>);

impl StateListener {
    /// A callback that finishes before the next delivery starts.
    pub fn sync(
        listener: impl Fn(&Json, &Context, ReplicatedStateDelivery) -> ChordResult<()> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(move |value, context, delivery| ListenerOutcome::Done(listener(&value, &context, delivery))))
    }

    /// A callback whose future must settle before the next delivery to this subscription starts.
    pub fn asynchronous<F, Fut>(listener: F) -> Self
    where
        F: Fn(Json, Context, ReplicatedStateDelivery) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ChordResult<()>> + Send + 'static,
    {
        Self(Arc::new(move |value, context, delivery| {
            ListenerOutcome::Pending(Box::pin(listener(value, context, delivery)))
        }))
    }
}

/// Receives failures a component cannot return to a caller.
pub type ErrorReporter = Arc<dyn Fn(ChordError) + Send + Sync>;

/// Upstream reports listener failures without an owner by throwing in a microtask.
pub(crate) fn unhandled_reporter() -> ErrorReporter {
    Arc::new(|error| report_unhandled(&error))
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub value: Json,
    pub context: Context,
    pub delivery: ReplicatedStateDelivery,
}

#[derive(Default)]
struct Queue {
    pending: VecDeque<Frame>,
    running: bool,
    started: bool,
    closed: bool,
}

/// One public subscription, independent of producer and other subscriber progress.
pub(crate) struct StateSubscriber {
    listener: StateListener,
    report_error: ErrorReporter,
    queue: Mutex<Queue>,
}

pub(crate) const MAX_PENDING_DELIVERIES: usize = 100;

impl StateSubscriber {
    pub fn new(listener: StateListener, report_error: ErrorReporter) -> Arc<Self> {
        Arc::new(Self { listener, report_error, queue: Mutex::new(Queue::default()) })
    }

    pub fn push(&self, frame: Frame) {
        let mut queue = lock(&self.queue);
        if queue.closed {
            return;
        }
        if queue.pending.len() == MAX_PENDING_DELIVERIES {
            // A cold replica can queue updates reentrantly before the first hydration starts.
            let hydration = if queue.started { None } else { queue.pending.pop_front() };
            queue.pending.clear();
            queue.pending.extend(hydration);
        }
        queue.pending.push_back(frame);
    }

    pub fn drain(self: &Arc<Self>) {
        {
            let mut queue = lock(&self.queue);
            if queue.running || queue.closed {
                return;
            }
            queue.running = true;
        }
        loop {
            let frame = {
                let mut queue = lock(&self.queue);
                match queue.pending.pop_front() {
                    Some(frame) => {
                        queue.started = true;
                        frame
                    }
                    None => {
                        queue.running = false;
                        return;
                    }
                }
            };
            match (self.listener.0)(frame.value, frame.context, frame.delivery) {
                ListenerOutcome::Done(result) => self.settle(result),
                ListenerOutcome::Pending(mut future) => {
                    if let Some(result) = (&mut future).now_or_never() {
                        self.settle(result);
                        continue;
                    }
                    let this = Arc::clone(self);
                    spawn_eager(async move {
                        let result = future.await;
                        this.settle(result);
                        lock(&this.queue).running = false;
                        this.drain();
                    });
                    return;
                }
            }
        }
    }

    fn settle(&self, result: ChordResult<()>) {
        if let Err(error) = result {
            (self.report_error)(error);
        }
    }

    pub fn clear(&self) {
        lock(&self.queue).pending.clear();
    }

    pub fn close(&self) {
        let mut queue = lock(&self.queue);
        queue.closed = true;
        queue.pending.clear();
    }
}
