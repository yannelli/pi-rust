//! Go-like invocation context: cancellation plus typed, immutable, invocation-scoped values.

use std::any::Any;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use tokio::sync::Notify;

use crate::error::{ChordError, ChordResult};
use crate::sync::lock;

type AbortListener = Box<dyn FnOnce(&ChordError) + Send>;

#[derive(Default)]
struct SignalState {
    reason: Option<ChordError>,
    listeners: Vec<(u64, AbortListener)>,
    next_listener: u64,
}

struct SignalInner {
    state: Mutex<SignalState>,
    notify: Notify,
}

/// A cancellation signal, the counterpart of the web `AbortSignal`.
#[derive(Clone)]
pub struct AbortSignal(Arc<SignalInner>);

impl AbortSignal {
    fn new() -> Self {
        Self(Arc::new(SignalInner { state: Mutex::new(SignalState::default()), notify: Notify::new() }))
    }

    pub fn aborted(&self) -> bool {
        lock(&self.0.state).reason.is_some()
    }

    /// The abort reason; `ChordError::Aborted` when none was supplied.
    pub fn reason(&self) -> Option<ChordError> {
        lock(&self.0.state).reason.clone()
    }

    /// Fail with the abort reason when aborted.
    pub fn throw_if_aborted(&self) -> ChordResult<()> {
        self.reason().map_or(Ok(()), Err)
    }

    /// Run `listener` once on abort. It runs immediately when already aborted.
    pub fn add_listener(&self, listener: impl FnOnce(&ChordError) + Send + 'static) -> u64 {
        let mut state = lock(&self.0.state);
        if let Some(reason) = state.reason.clone() {
            drop(state);
            listener(&reason);
            return 0;
        }
        state.next_listener += 1;
        let id = state.next_listener;
        state.listeners.push((id, Box::new(listener)));
        id
    }

    pub fn remove_listener(&self, id: u64) {
        lock(&self.0.state).listeners.retain(|(existing, _)| *existing != id);
    }

    /// Resolve once the signal aborts, with its reason.
    pub async fn cancelled(&self) -> ChordError {
        loop {
            let notified = self.0.notify.notified();
            if let Some(reason) = self.reason() {
                return reason;
            }
            notified.await;
        }
    }

    fn abort(&self, reason: Option<ChordError>) {
        let listeners = {
            let mut state = lock(&self.0.state);
            if state.reason.is_some() {
                return;
            }
            state.reason = Some(reason.unwrap_or(ChordError::Aborted));
            std::mem::take(&mut state.listeners)
        };
        self.0.notify.notify_waiters();
        let reason = self.reason().unwrap_or(ChordError::Aborted);
        for (_, listener) in listeners {
            listener(&reason);
        }
    }

    /// A signal that aborts when any input aborts, with that input's reason.
    pub fn any(signals: &[AbortSignal]) -> AbortSignal {
        let combined = AbortSignal::new();
        for signal in signals {
            if let Some(reason) = signal.reason() {
                combined.abort(Some(reason));
                return combined;
            }
        }
        for signal in signals {
            let weak: Weak<SignalInner> = Arc::downgrade(&combined.0);
            signal.add_listener(move |reason| {
                if let Some(inner) = weak.upgrade() {
                    AbortSignal(inner).abort(Some(reason.clone()));
                }
            });
        }
        combined
    }

    pub fn ptr_eq(&self, other: &AbortSignal) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Debug for AbortSignal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbortSignal").field("aborted", &self.aborted()).finish()
    }
}

/// Owns an [`AbortSignal`] and aborts it.
#[derive(Clone, Debug)]
pub struct AbortController {
    signal: AbortSignal,
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortController {
    pub fn new() -> Self {
        Self { signal: AbortSignal::new() }
    }

    pub fn signal(&self) -> AbortSignal {
        self.signal.clone()
    }

    /// Abort with an optional reason. Later calls have no effect.
    pub fn abort(&self, reason: Option<ChordError>) {
        self.signal.abort(reason);
    }
}

static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

/// Typed identity for one value carried by a [`Context`].
pub struct ContextKey<T> {
    id: u64,
    description: Arc<str>,
    marker: PhantomData<fn(T) -> T>,
}

impl<T> Clone for ContextKey<T> {
    fn clone(&self) -> Self {
        Self { id: self.id, description: self.description.clone(), marker: PhantomData }
    }
}

impl<T> ContextKey<T> {
    pub fn description(&self) -> &str {
        &self.description
    }
}

/// Create a fresh key; two keys never collide even with equal descriptions.
pub fn create_context_key<T>(description: &str) -> ContextKey<T> {
    ContextKey { id: NEXT_KEY.fetch_add(1, Ordering::Relaxed), description: description.into(), marker: PhantomData }
}

enum Node {
    Empty(&'static str),
    Value { parent: Context, key: u64, description: Arc<str>, value: Arc<dyn Any + Send + Sync> },
}

/// Immutable invocation-scoped values passed explicitly through operations.
#[derive(Clone)]
pub struct Context(Arc<Node>);

static ABORT_SIGNAL_KEY: LazyLock<ContextKey<Option<AbortSignal>>> =
    LazyLock::new(|| create_context_key("chord.abortSignal"));

/// The root context for work without a caller.
pub static BACKGROUND_CONTEXT: LazyLock<Context> =
    LazyLock::new(|| Context(Arc::new(Node::Empty("[Context BACKGROUND_CONTEXT]"))));

/// A placeholder root for code that has not yet been given a context.
pub static TODO_CONTEXT: LazyLock<Context> = LazyLock::new(|| Context(Arc::new(Node::Empty("[Context TODO_CONTEXT]"))));

impl Context {
    /// The background root context.
    pub fn background() -> Context {
        BACKGROUND_CONTEXT.clone()
    }

    /// The value stored under `key`, searching from the innermost layer.
    pub fn value<T: Send + Sync + 'static>(&self, key: &ContextKey<T>) -> Option<&T> {
        let mut node = &self.0;
        loop {
            match &**node {
                Node::Empty(_) => return None,
                Node::Value { parent, key: id, value, .. } => {
                    if *id == key.id {
                        return value.downcast_ref::<T>();
                    }
                    node = &parent.0;
                }
            }
        }
    }

    pub fn abort_signal(&self) -> Option<AbortSignal> {
        self.value(&ABORT_SIGNAL_KEY).cloned().flatten()
    }

    /// Whether this context's signal has aborted.
    pub fn is_cancelled(&self) -> bool {
        self.abort_signal().is_some_and(|signal| signal.aborted())
    }

    pub fn ptr_eq(&self, other: &Context) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl fmt::Display for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &*self.0 {
            Node::Empty(name) => f.write_str(name),
            Node::Value { parent, description, .. } => write!(f, "{parent}.WithValue({description})"),
        }
    }
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Derive a context with one value set or replaced.
pub fn with_context_value<T: Send + Sync + 'static>(key: &ContextKey<T>, value: T, parent: &Context) -> Context {
    Context(Arc::new(Node::Value {
        parent: parent.clone(),
        key: key.id,
        description: key.description.clone(),
        value: Arc::new(value),
    }))
}

/// Derive a context cancelled by either the parent signal or `signal`.
pub fn with_abort_signal(signal: &AbortSignal, context: &Context) -> Context {
    let combined = match context.abort_signal() {
        None => signal.clone(),
        Some(parent) => AbortSignal::any(&[parent, signal.clone()]),
    };
    with_context_value(&ABORT_SIGNAL_KEY, Some(combined), context)
}

/// Derive a context retaining all values except caller cancellation, for mandatory cleanup only.
pub fn without_abort_signal(context: &Context) -> Context {
    with_context_value(&ABORT_SIGNAL_KEY, None, context)
}

/// Derive an independently cancellable child context and the controller that cancels it.
pub fn with_cancel(context: &Context) -> (Context, AbortController) {
    let controller = AbortController::new();
    (with_abort_signal(&controller.signal(), context), controller)
}

/// Await `future` until it settles or `context` is cancelled. Cancellation fails only this waiter.
pub async fn await_with_context<T>(future: impl Future<Output = ChordResult<T>>, context: &Context) -> ChordResult<T> {
    let Some(signal) = context.abort_signal() else {
        return future.await;
    };
    signal.throw_if_aborted()?;
    tokio::select! {
        biased;
        reason = signal.cancelled() => Err(reason),
        result = future => result,
    }
}
