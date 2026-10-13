//! Small concurrency helpers: poison-tolerant locks, eager task starts, and disposers.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use futures::FutureExt;
use futures::future::Shared;

use crate::error::{ChordError, ChordResult};

/// A boxed, sendable future.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A started task whose result any number of waiters can share.
pub type SharedTask = Shared<BoxFuture<ChordResult<()>>>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// Poll `future` once on the calling thread, then keep driving it on the Tokio runtime.
///
/// This mirrors JavaScript, where an async function runs synchronously up to its first await.
pub(crate) fn spawn_eager<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut future: BoxFuture<()> = Box::pin(future);
    if (&mut future).now_or_never().is_some() {
        return;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(future);
        }
        Err(_) => {
            std::thread::spawn(move || futures::executor::block_on(future));
        }
    }
}

/// Start a fallible task eagerly and return a handle that every waiter can await.
pub(crate) fn start_shared<F>(future: F) -> SharedTask
where
    F: Future<Output = ChordResult<()>> + Send + 'static,
{
    let boxed: BoxFuture<ChordResult<()>> = Box::pin(future);
    let shared = boxed.shared();
    let driver = shared.clone();
    spawn_eager(async move {
        let _ = driver.await;
    });
    shared
}

/// Return to the scheduler once, like a JavaScript `await` on a settled promise.
pub(crate) async fn yield_now() {
    let mut yielded = false;
    std::future::poll_fn(|context| {
        if yielded {
            return std::task::Poll::Ready(());
        }
        yielded = true;
        context.waker().wake_by_ref();
        std::task::Poll::Pending
    })
    .await
}

/// A resolved shared task.
pub(crate) fn ready_task(result: ChordResult<()>) -> SharedTask {
    let boxed: BoxFuture<ChordResult<()>> = Box::pin(std::future::ready(result));
    boxed.shared()
}

/// An idempotent disposer returned by subscriptions, observations, and instance spawns.
#[derive(Clone)]
pub struct Unsubscribe {
    done: Arc<AtomicBool>,
    action: Arc<dyn Fn() -> ChordResult<()> + Send + Sync>,
}

impl Unsubscribe {
    pub fn new(action: impl Fn() + Send + Sync + 'static) -> Self {
        Self::fallible(move || {
            action();
            Ok(())
        })
    }

    /// A disposer whose cleanup can fail, such as closing a published instance.
    pub fn fallible(action: impl Fn() -> ChordResult<()> + Send + Sync + 'static) -> Self {
        Self { done: Arc::new(AtomicBool::new(false)), action: Arc::new(action) }
    }

    /// A disposer that does nothing.
    pub fn noop() -> Self {
        Self::new(|| {})
    }

    /// Run the disposal once and return its failure; later calls do nothing.
    pub fn close(&self) -> ChordResult<()> {
        if self.done.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        (self.action)()
    }

    /// Like [`Unsubscribe::close`], sending any failure to the unhandled-error hook.
    pub fn unsubscribe(&self) {
        if let Err(error) = self.close() {
            crate::error::report_unhandled(&error);
        }
    }
}

impl std::fmt::Debug for Unsubscribe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Unsubscribe").field("done", &self.done.load(Ordering::SeqCst)).finish()
    }
}

/// Await every future, then report all failures together.
pub(crate) async fn settle_all(futures: Vec<BoxFuture<ChordResult<()>>>, message: &str) -> ChordResult<()> {
    let results = futures::future::join_all(futures).await;
    let errors: Vec<ChordError> = results.into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        return Ok(());
    }
    Err(ChordError::Aggregate { message: message.to_string(), errors })
}
