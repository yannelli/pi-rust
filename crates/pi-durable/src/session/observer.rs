//! Synchronous observers of committed state for adapters (upstream `Observer` and `Observation`), and the guard the
//! Session puts in front of a host's observer.

use std::sync::{Arc, Mutex};

use pi_chord::{Context, Json};

use super::line::lock;
use super::observation::CommittedObserver;
use super::{WatchCall, WatchEnd};
use crate::errors::{DurableError, DurableResult};

/// A synchronous observer of committed state with Chord's watch shapes (`observe_doc()`, `observe_conversation()`,
/// `observe_task_graph()`). `advance` gets every change, unfolded, as memory adopts it: record it, never block.
pub trait Observer: Send + Sync {
    /// One call; its ops apply from the value before it. Durable's failures are final, so every call is a `Value`.
    /// An error is reported as `listener_error`, detaches the observer, and ends it `ListenerError`.
    fn advance(&self, call: &WatchCall, context: &Context) -> DurableResult<()>;
    /// Called once: `Ended` when the Session closes or the owner of the scope ends, `Failed` with the error that
    /// failed the Session; never after `Observation::detach()`.
    fn end(&self, end: WatchEnd);
}

/// An attached [`Observer`]: the value at attachment, which every later `advance` starts from, and how to detach it.
#[derive(Clone)]
pub struct Observation {
    pub value: Json,
    detach: Arc<dyn Fn() + Send + Sync>,
}

impl Observation {
    pub(crate) fn new(value: Json, detach: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self { value, detach }
    }

    /// Idempotent: no `advance` or `end` follows it, not even for the rest of a commit underway.
    pub fn detach(&self) {
        (self.detach)();
    }
}

impl std::fmt::Debug for Observation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observation").field("value", &self.value).finish_non_exhaustive()
    }
}

type Report = Arc<dyn Fn(&DurableError) + Send + Sync>;

struct Attached {
    observer: Arc<dyn Observer>,
    detach: Box<dyn FnOnce() + Send>,
    report: Report,
}

/// A host's observer behind the rule for observers that run inside Storage's `adopt`: one that fails is reported,
/// detached, and ended with the error; nothing reaches it after its detach or end, and from then on the guard
/// references neither it nor its source.
pub(crate) struct GuardedObserver(Mutex<Option<Attached>>);

impl GuardedObserver {
    pub fn new(
        observer: Arc<dyn Observer>,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(&DurableError) + Send + Sync + 'static,
    ) -> Arc<Self> {
        let attached = Attached { observer, detach: Box::new(detach), report: Arc::new(report) };
        Arc::new(Self(Mutex::new(Some(attached))))
    }

    /// Detach from the source once, and hand back what it was attached with.
    fn release(&self) -> Option<(Arc<dyn Observer>, Report)> {
        let attached = lock(&self.0).take()?;
        (attached.detach)();
        Some((attached.observer, attached.report))
    }

    pub fn detach(&self) {
        self.release();
    }
}

impl CommittedObserver for GuardedObserver {
    fn advance(&self, call: WatchCall, context: Context) {
        let current = lock(&self.0).as_ref().map(|attached| (attached.observer.clone(), attached.report.clone()));
        let Some((observer, report)) = current else { return };
        if let Err(error) = observer.advance(&call, &context) {
            report(&error);
            if self.release().is_some() {
                observer.end(WatchEnd::ListenerError(error));
            }
        }
    }

    fn end(&self, end: WatchEnd) {
        if let Some((observer, _)) = self.release() {
            observer.end(end);
        }
    }

    fn owner_ended(&self) {
        self.end(WatchEnd::Ended);
    }

    fn keeps_cancellation(&self) -> bool {
        true
    }
}
