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

/// Terminal result of one watch. `E` is the error type of the source; upstream errors are untyped.
#[derive(Clone, Debug)]
pub enum WatchEnd<E = ChordError> {
    /// `stop()` was called.
    Stopped,
    /// The context the watch was acquired with was cancelled.
    Cancelled,
    /// The source was retired; its last frame was `[["r", null]]`.
    Retired,
    /// The source closed (its owner ended or closed).
    Ended,
    /// The listener failed with the error.
    ListenerError(E),
    /// The source failed with the error.
    Failed(E),
}

impl<E> WatchEnd<E> {
    /// The upstream `reason` string.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Cancelled => "cancelled",
            Self::Retired => "retired",
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

/// Serialized exact-frame observation of an immutable value with bounded pending delivery.
pub trait WatchHandle<T = Json, E = ChordError> {
    /// Acquisition revision before start; latest delivered immutable revision afterward.
    fn value(&self) -> T;

    /// Install the sole asynchronous listener. Never invokes it inline.
    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(T, Vec<Op>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static;

    /// Idempotently stop future callbacks and return this watch's terminal result.
    fn stop(&self) -> BoxFuture<WatchEnd<E>>;

    /// Settle when the watch terminates; an already-running callback remains caller-owned.
    fn closed(&self) -> BoxFuture<WatchEnd<E>>;
}

/// Serialized observation of an in-process value: as [`WatchHandle`], but a change is a new value without ops.
pub trait NativeWatchHandle<T, E = ChordError> {
    /// Acquisition value before start; latest delivered value afterward.
    fn value(&self) -> T;

    /// Install the sole asynchronous listener. Never invokes it inline.
    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(T, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static;

    /// Idempotently stop future callbacks and return this watch's terminal result.
    fn stop(&self) -> BoxFuture<WatchEnd<E>>;

    /// Settle when the watch terminates; an already-running callback remains caller-owned.
    fn closed(&self) -> BoxFuture<WatchEnd<E>>;
}

/// A value that changes and can be watched: documents, derives, members of references. Values are immutable by
/// convention (JSON for state); each watch starts at the current value and then gets one frame per change.
pub trait Watchable<T = Json, E = ChordError> {
    type Handle: WatchHandle<T, E>;

    /// The current value.
    fn value(&self) -> T;

    /// A watch holding the current value, then one frame per change; the context's cancellation ends it.
    fn watch(&self, context: &Context) -> BoxFuture<Result<Self::Handle, E>>;
}

/// An in-process value that changes and can be watched (it may hold anything, errors and functions included): a change
/// is a new value, delivered without ops.
pub trait NativeWatchable<T, E = ChordError> {
    type Handle: NativeWatchHandle<T, E>;

    /// The current value.
    fn value(&self) -> T;

    /// A watch holding the current value, then one frame per new value; the context's cancellation ends it.
    fn watch(&self, context: &Context) -> BoxFuture<Result<Self::Handle, E>>;
}
