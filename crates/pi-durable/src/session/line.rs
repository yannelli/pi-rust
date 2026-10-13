//! The Session mutation line and small async helpers. Jobs enter the line in call order and start eagerly on
//! Tokio, like JavaScript promises: a caller that drops its future does not cancel an admitted job.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt;
use futures::future::Shared;
use pi_chord::{BoxFuture, Context};
use tokio::sync::{Notify, oneshot};

use crate::errors::{DurableError, DurableResult};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// A started operation whose result any number of waiters can share.
pub type SharedResult<T> = Shared<BoxFuture<DurableResult<T>>>;

/// Spawn `future` now and return a handle every waiter can await.
pub(crate) fn spawn_shared<T, F>(future: F) -> SharedResult<T>
where
    T: Clone + Send + Sync + 'static,
    F: Future<Output = DurableResult<T>> + Send + 'static,
{
    let boxed: BoxFuture<DurableResult<T>> = Box::pin(future);
    let shared = boxed.shared();
    let driver = shared.clone();
    tokio::spawn(async move {
        let _ = driver.await;
    });
    shared
}

/// A settled shared result.
pub(crate) fn ready_shared<T>(result: DurableResult<T>) -> SharedResult<T>
where
    T: Clone + Send + Sync + 'static,
{
    let boxed: BoxFuture<DurableResult<T>> = Box::pin(std::future::ready(result));
    boxed.shared()
}

/// Map the end of a spawned job.
pub(crate) fn joined<T>(result: Result<T, tokio::task::JoinError>) -> DurableResult<T> {
    result.map_err(|error| DurableError::error(format!("Session job failed: {error}")))
}

/// Await `future` until it settles or `context` is cancelled; cancellation fails only this waiter.
pub(crate) async fn await_with_context<T>(
    future: impl Future<Output = DurableResult<T>>,
    context: &Context,
) -> DurableResult<T> {
    let Some(signal) = context.abort_signal() else {
        return future.await;
    };
    signal.throw_if_aborted()?;
    tokio::select! {
        biased;
        reason = signal.cancelled() => Err(reason.into()),
        result = future => result,
    }
}

/// One mutation line: jobs run one at a time in enqueue order.
pub(crate) struct Line {
    tail: Mutex<Shared<BoxFuture<()>>>,
}

impl Line {
    pub fn new() -> Self {
        let ready: BoxFuture<()> = Box::pin(std::future::ready(()));
        Self { tail: Mutex::new(ready.shared()) }
    }

    /// Queue `job` behind every job enqueued before and start waiting now.
    pub fn enqueue<T, F>(&self, job: F) -> impl Future<Output = DurableResult<T>> + Send + 'static
    where
        T: Send + 'static,
        F: Future<Output = DurableResult<T>> + Send + 'static,
    {
        let (done, released) = oneshot::channel::<()>();
        let next: BoxFuture<()> = Box::pin(async move {
            let _ = released.await;
        });
        let previous = std::mem::replace(&mut *lock(&self.tail), next.shared());
        let handle = tokio::spawn(async move {
            previous.await;
            let result = job.await;
            drop(done);
            result
        });
        async move { joined(handle.await)? }
    }
}

/// Counter of unfinished operations with a wait for zero.
#[derive(Default)]
pub(crate) struct PendingCounter {
    count: AtomicUsize,
    idle: Notify,
}

impl PendingCounter {
    pub fn enter(self: &Arc<Self>) -> PendingGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        PendingGuard(self.clone())
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub async fn drained(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count() == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Leaves the counter when dropped.
pub(crate) struct PendingGuard(Arc<PendingCounter>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}
