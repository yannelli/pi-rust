//! Adaptive progress commits (upstream `Progress` of `harness/output.ts`): the first change after an idle period
//! commits at once; each commit delays the next by at least the interval and by its size at 100 KiB/s.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::util::lock;
use crate::errors::{DurableError, DurableResult};

/// Each progress commit also buys a pause proportional to what it wrote.
pub const PROGRESS_BYTES_PER_SECOND: f64 = 100.0 * 1024.0;

type WriteFn = Arc<dyn Fn() -> BoxFuture<'static, DurableResult<u64>> + Send + Sync>;
type ErrorFn = Arc<dyn Fn(&DurableError) + Send + Sync>;
/// Settles with the commit that includes a change.
pub type ProgressWaiter = oneshot::Sender<DurableResult<()>>;

struct State {
    waiters: Vec<ProgressWaiter>,
    timer: Option<JoinHandle<()>>,
    in_flight: bool,
    next_at: Instant,
    dirty: bool,
    stopped: bool,
}

struct Inner {
    write: WriteFn,
    on_error: ErrorFn,
    min_interval_ms: u64,
    state: Mutex<State>,
    idle: watch::Sender<bool>,
}

/// At most one commit is in flight; changes made meanwhile coalesce into the next one. Cheap to clone.
#[derive(Clone)]
pub struct Progress(Arc<Inner>);

impl Progress {
    pub fn new(
        write: impl Fn() -> BoxFuture<'static, DurableResult<u64>> + Send + Sync + 'static,
        on_error: impl Fn(&DurableError) + Send + Sync + 'static,
        min_interval_ms: u64,
    ) -> Self {
        let state = State {
            waiters: Vec::new(),
            timer: None,
            in_flight: false,
            next_at: Instant::now(),
            dirty: false,
            stopped: false,
        };
        let (idle, _) = watch::channel(true);
        Self(Arc::new(Inner {
            write: Arc::new(write),
            on_error: Arc::new(on_error),
            min_interval_ms,
            state: Mutex::new(state),
            idle,
        }))
    }

    /// Schedule a commit.
    pub fn mark(&self) {
        lock(&self.0.state).dirty = true;
        self.schedule();
    }

    /// Schedule a commit; the receiver settles with the commit that includes this change.
    pub fn mark_and_wait(&self) -> oneshot::Receiver<DurableResult<()>> {
        let (sender, receiver) = oneshot::channel();
        lock(&self.0.state).waiters.push(sender);
        self.mark();
        receiver
    }

    /// Stop committing and wait for the commit in flight; returns the waiters the final commit must settle.
    pub async fn stop(&self) -> Vec<ProgressWaiter> {
        {
            let mut state = lock(&self.0.state);
            state.stopped = true;
            if let Some(timer) = state.timer.take() {
                timer.abort();
            }
        }
        let mut idle = self.0.idle.subscribe();
        let _ = idle.wait_for(|idle| *idle).await;
        std::mem::take(&mut lock(&self.0.state).waiters)
    }

    fn schedule(&self) {
        let wait = {
            let mut state = lock(&self.0.state);
            if state.stopped || state.timer.is_some() || state.in_flight {
                return;
            }
            let now = Instant::now();
            if state.next_at <= now {
                None
            } else {
                let progress = self.clone();
                let wait = state.next_at - now;
                state.timer = Some(tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    lock(&progress.0.state).timer = None;
                    progress.flush();
                }));
                Some(())
            }
        };
        if wait.is_none() {
            self.flush();
        }
    }

    fn flush(&self) {
        let waiters = {
            let mut state = lock(&self.0.state);
            if state.stopped || !state.dirty {
                return;
            }
            state.dirty = false;
            state.in_flight = true;
            std::mem::take(&mut state.waiters)
        };
        self.0.idle.send_replace(false);
        let progress = self.clone();
        let started = Instant::now();
        let write = (self.0.write)();
        tokio::spawn(async move {
            let result = write.await;
            let min = Duration::from_millis(progress.0.min_interval_ms);
            let pause = match &result {
                Ok(bytes) => min.max(Duration::from_secs_f64(*bytes as f64 / PROGRESS_BYTES_PER_SECOND)),
                Err(_) => min,
            };
            lock(&progress.0.state).next_at = started + pause;
            match result {
                Ok(_) => {
                    for waiter in waiters {
                        let _ = waiter.send(Ok(()));
                    }
                }
                Err(error) => {
                    for waiter in waiters {
                        let _ = waiter.send(Err(error.clone()));
                    }
                    (progress.0.on_error)(&error);
                }
            }
            let dirty = {
                let mut state = lock(&progress.0.state);
                state.in_flight = false;
                state.dirty
            };
            progress.0.idle.send_replace(true);
            if dirty {
                progress.schedule();
            }
        });
    }
}
