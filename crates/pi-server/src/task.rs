//! Small task helpers: eagerly started shared futures and poison-tolerant locks.

use std::future::Future;
use std::sync::{Mutex, MutexGuard};

use futures::FutureExt;
use futures::future::Shared;
use pi_chord::{BoxFuture, ChordError, ChordResult};

pub(crate) type SharedTask<T> = Shared<BoxFuture<T>>;

/// Start `future` on the runtime now and return a handle any number of waiters can await.
pub(crate) fn spawn_shared<T, F>(future: F) -> SharedTask<T>
where
    T: Clone + Send + Sync + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let boxed: BoxFuture<T> = Box::pin(future);
    let shared = boxed.shared();
    tokio::spawn(shared.clone());
    shared
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Collapse failures: none is `Ok`, one is itself, more are an aggregate.
pub(crate) fn collect_errors(errors: Vec<ChordError>, message: &str) -> ChordResult<()> {
    ChordError::collect(errors, message)
}
