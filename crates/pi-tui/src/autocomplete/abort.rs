//! Cancellation for autocomplete requests (the `AbortController` and `AbortSignal` that upstream
//! passes to providers).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

type AbortListener = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct AbortState {
    aborted: AtomicBool,
    listeners: Mutex<Vec<AbortListener>>,
}

/// Read side of an [`AbortController`]. Cheap to clone and `Send + Sync`, so background work can
/// poll it or register a listener.
#[derive(Clone, Default)]
pub struct AbortSignal {
    state: Arc<AbortState>,
}

impl std::fmt::Debug for AbortSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AbortSignal").field("aborted", &self.is_aborted()).finish()
    }
}

impl AbortSignal {
    /// Whether the request was aborted.
    pub fn is_aborted(&self) -> bool {
        self.state.aborted.load(Ordering::SeqCst)
    }

    /// Runs `listener` once when the signal aborts, or right away when it already has
    /// (upstream `addEventListener("abort", ..., { once: true })`).
    pub fn on_abort(&self, listener: impl FnOnce() + Send + 'static) {
        {
            let mut listeners = self.state.listeners.lock().unwrap_or_else(PoisonError::into_inner);
            if !self.is_aborted() {
                listeners.push(Box::new(listener));
                return;
            }
        }
        listener();
    }
}

/// Aborts the requests that hold its [`AbortSignal`].
#[derive(Clone, Default, Debug)]
pub struct AbortController {
    signal: AbortSignal,
}

impl AbortController {
    pub fn new() -> Self {
        Self::default()
    }

    /// The signal to hand to the request.
    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Marks the signal aborted and runs its listeners on this thread. Later calls do nothing.
    pub fn abort(&self) {
        let state = &self.signal.state;
        if state.aborted.swap(true, Ordering::SeqCst) {
            return;
        }
        let listeners = std::mem::take(&mut *state.listeners.lock().unwrap_or_else(PoisonError::into_inner));
        for listener in listeners {
            listener();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn runs_listeners_once() {
        let controller = AbortController::new();
        let signal = controller.signal();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        signal.on_abort(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert!(!signal.is_aborted());
        controller.abort();
        controller.abort();
        assert!(signal.is_aborted());
        assert_eq!(count.load(Ordering::SeqCst), 1);

        let counter = count.clone();
        signal.on_abort(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
