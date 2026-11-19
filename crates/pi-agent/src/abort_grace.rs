//! How long an aborted run still waits for a tool, a hook, or a listener (pi-rust). Upstream
//! awaits each one to the end, so a slow extension handler or a tool that ignores the abort
//! signal can keep `abort()` from returning for seconds. Here every wait after an abort ends
//! [`ABORT_GRACE`] after the abort: a tool's result becomes an abort error, and a hook or
//! listener keeps running on its own with its result unused. Runs that are not aborted wait as
//! upstream does.

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How long after an abort a run still waits for its tools, hooks, and listeners.
pub const ABORT_GRACE: Duration = Duration::from_millis(400);

/// When a run was aborted, shared by the waits of that run so that together they end
/// [`ABORT_GRACE`] after the abort.
#[derive(Clone, Default)]
pub struct AbortClock(Arc<Mutex<Option<Instant>>>);

impl AbortClock {
    /// Record the abort; a later call keeps the first time.
    pub fn mark(&self) {
        self.slot().get_or_insert_with(Instant::now);
    }

    pub fn reset(&self) {
        *self.slot() = None;
    }

    /// The end of the grace period, once the run was aborted.
    pub fn deadline(&self) -> Option<Instant> {
        self.slot().map(|at| at + ABORT_GRACE)
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Resolves once `cancel` fired and the grace period of `clock` (or [`ABORT_GRACE`] from now,
/// when the clock has no abort time) is over.
pub(crate) async fn grace_elapsed(cancel: &CancellationToken, clock: &AbortClock) {
    cancel.cancelled().await;
    let deadline = clock.deadline().unwrap_or_else(|| Instant::now() + ABORT_GRACE);
    tokio::time::sleep_until(deadline).await;
}

/// Await `future`, or leave it running detached once `grace` resolves and return `None`.
pub(crate) async fn finish_or_detach<T: Send + 'static>(
    mut future: BoxFuture<'static, T>,
    grace: impl Future<Output = ()>,
) -> Option<T> {
    tokio::select! {
        biased;
        output = &mut future => Some(output),
        () = grace => {
            tokio::spawn(future);
            None
        }
    }
}

/// Await `future`, or drop it once `grace` resolves and return `None`.
pub(crate) async fn finish_or_drop<T>(future: impl Future<Output = T>, grace: impl Future<Output = ()>) -> Option<T> {
    tokio::select! {
        biased;
        output = future => Some(output),
        () = grace => None,
    }
}
