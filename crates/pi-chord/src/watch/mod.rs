//! Watch handles: the watch types and the one implementation of their lifecycle, for JSON and native values
//! (upstream `watch.ts`).

mod handle;
mod lifecycle;

use std::future::Future;

use crate::context::Context;
use crate::delta::Op;
use crate::error::ChordError;
use crate::json::Json;
use crate::sync::BoxFuture;

pub use handle::{NativeWatch, Watch};
pub use lifecycle::Replace;

/// Terminal result of one watch, tagged by `kind`; a recoverable failure is a `Failed` call ([`WatchCall`]) instead.
/// `E` is the error type of the source; upstream errors are untyped.
#[derive(Clone, Debug)]
pub enum WatchEnd<E = ChordError> {
    /// `stop()` was called.
    Stopped,
    /// The context the watch was acquired with was aborted.
    Aborted,
    /// The source closed (its owner ended or closed).
    Ended,
    /// The listener failed with the error.
    ListenerError(E),
    /// The source failed for good with the error.
    Failed(E),
}

impl<E> WatchEnd<E> {
    /// The upstream `kind` string.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Aborted => "aborted",
            Self::Ended => "ended",
            Self::ListenerError(_) => "listener_error",
            Self::Failed(_) => "failed",
        }
    }

    pub fn error(&self) -> Option<&E> {
        match self {
            Self::ListenerError(error) | Self::Failed(error) => Some(error),
            _ => None,
        }
    }
}

/// One call of a JSON watch's listener and one frame a source hands it: a value with its ops and retirement flag, or a
/// failure the watch stays open through. Plain data: the frame's context is the listener's second argument.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchCall<T = Json, E = ChordError> {
    Value { value: T, ops: Vec<Op>, retired: bool },
    Failed { error: E },
}

impl<T, E> WatchCall<T, E> {
    /// A source's value frame that is not a retirement.
    pub fn new(value: T, ops: Vec<Op>) -> Self {
        Self::Value { value, ops, retired: false }
    }

    /// The upstream `kind` string.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Value { .. } => "value",
            Self::Failed { .. } => "failed",
        }
    }
}

/// One call of a native watch's listener: the new value, or a failure its source may recover from, as in
/// [`WatchCall`].
#[derive(Clone, Debug, PartialEq)]
pub enum NativeWatchCall<T, E = ChordError> {
    Value { value: T },
    Failed { error: E },
}

impl<T, E> NativeWatchCall<T, E> {
    pub fn new(value: T) -> Self {
        Self::Value { value }
    }

    /// The upstream `kind` string.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Value { .. } => "value",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Serialized observation of an immutable value: one listener call at a time; changes made meanwhile arrive folded into
/// the next call.
pub trait WatchHandle<T = Json, E = ChordError> {
    /// Acquisition revision before start; latest delivered immutable revision afterward (a `Failed` call keeps it).
    fn value(&self) -> T;

    /// Install the sole asynchronous listener. Never invokes it inline.
    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(WatchCall<T, E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static;

    /// Idempotently stop future callbacks and return this watch's terminal result.
    fn stop(&self) -> BoxFuture<WatchEnd<E>>;

    /// Settle when the watch terminates; an already-running callback remains caller-owned.
    fn closed(&self) -> BoxFuture<WatchEnd<E>>;
}

/// Serialized observation of an in-process value: as [`WatchHandle`], but each new value is its own call, in order,
/// without ops.
pub trait NativeWatchHandle<T, E = ChordError> {
    /// Acquisition value before start; latest delivered value afterward (a `Failed` call keeps it).
    fn value(&self) -> T;

    /// Install the sole asynchronous listener. Never invokes it inline.
    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(NativeWatchCall<T, E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static;

    /// Idempotently stop future callbacks and return this watch's terminal result.
    fn stop(&self) -> BoxFuture<WatchEnd<E>>;

    /// Settle when the watch terminates; an already-running callback remains caller-owned.
    fn closed(&self) -> BoxFuture<WatchEnd<E>>;
}

/// A value that changes and can be watched: documents, derives, members of references. Values are immutable by
/// convention (JSON for state); each watch starts at the current value and then gets the changes, folded while its
/// listener is busy.
pub trait Watchable<T = Json, E = ChordError> {
    type Handle: WatchHandle<T, E>;

    /// The current value.
    fn value(&self) -> T;

    /// A watch holding the current value, then its changes; aborting the context ends it.
    fn watch(&self, context: &Context) -> BoxFuture<Result<Self::Handle, E>>;
}

/// An in-process value that changes and can be watched (it may hold anything, errors and functions included): a change
/// is a new value by identity, delivered without ops.
pub trait NativeWatchable<T, E = ChordError> {
    type Handle: NativeWatchHandle<T, E>;

    /// The current value.
    fn value(&self) -> T;

    /// A watch holding the current value, then one frame per new value; aborting the context ends it.
    fn watch(&self, context: &Context) -> BoxFuture<Result<Self::Handle, E>>;
}
