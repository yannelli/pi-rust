//! The watch handles sources create: `Watch` for JSON values with ops, `NativeWatch` for in-process values.

use std::future::Future;
use std::sync::Arc;

use super::lifecycle::{Lifecycle, Replace};
use super::{NativeWatchHandle, WatchEnd, WatchHandle};
use crate::context::{AbortSignal, Context};
use crate::delta::Op;
use crate::error::ChordError;
use crate::json::Json;
use crate::sync::BoxFuture;

/// Canonical terminal frame of a retired source.
fn retirement_operations() -> Vec<Op> {
    vec![Op::Replace(Json::Null)]
}

fn root_replacement(value: &Json) -> Vec<Op> {
    vec![Op::Replace(value.clone())]
}

fn no_ops<T>(_: &T) -> Vec<Op> {
    Vec::new()
}

/// The watch handle of a JSON source: exact frames in order; an overflow becomes one root replacement
/// `[["r", newest]]`. `detach` unsubscribes it from its source when it ends; `report` gets a listener's error, which
/// also ends the watch; `replace` gives the value an overflow delivers, by default the newest value. Cheap to clone.
pub struct Watch<E = ChordError>(Arc<Lifecycle<Json, E>>);

impl<E> Clone for Watch<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<E> Watch<E>
where
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    pub fn new(
        value: Json,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(&E) + Send + Sync + 'static,
        replace: Option<Replace<Json>>,
    ) -> Self {
        Self(Lifecycle::new(value, Box::new(detach), Arc::new(report), replace, root_replacement))
    }

    /// The acquisition value before `start()`, then the latest delivered value.
    pub fn value(&self) -> Json {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(Json, Vec<Op>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |value, ops, context| Box::pin(listener(value, ops, context))))
    }

    /// Idempotently stop future callbacks and return the end. A running callback keeps running.
    pub fn stop(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<E> {
        self.0.terminate(WatchEnd::Stopped);
        self.0.closed()
    }

    /// Settles when the watch ends.
    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<E> {
        self.0.closed()
    }

    /// End the watch `cancelled` when `signal` aborts; installed once.
    pub fn observe_cancellation(&self, signal: &AbortSignal) -> Result<(), E> {
        self.0.observe_cancellation(signal)
    }

    /// End the watch `cancelled`: its acquisition context was cancelled.
    pub fn cancel(&self) {
        self.0.terminate(WatchEnd::Cancelled);
    }

    /// The source closed: end with `ended`, or with `failed` and the error when the source failed.
    pub fn end(&self, failure: Option<E>) {
        self.0.terminate(failure.map_or(WatchEnd::Ended, WatchEnd::Failed));
    }

    /// Queue the frame `ops`, which turn the value before it into `value`.
    pub fn advance(&self, value: Json, ops: Vec<Op>, context: Context) {
        self.0.enqueue(value, ops, context);
    }

    /// The source retired: queue the last frame, `[["r", null]]` with value `null`, then end `retired`.
    pub fn retire(&self, context: Context) {
        self.0.enqueue_retirement(Json::Null, retirement_operations(), context);
    }
}

impl<E> WatchHandle<Json, E> for Watch<E>
where
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    fn value(&self) -> Json {
        Watch::value(self)
    }

    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(Json, Vec<Op>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        Watch::start(self, listener)
    }

    fn stop(&self) -> BoxFuture<WatchEnd<E>> {
        Box::pin(Watch::stop(self))
    }

    fn closed(&self) -> BoxFuture<WatchEnd<E>> {
        Box::pin(Watch::closed(self))
    }
}

/// The watch handle of a native source: values in order; an overflow collapses to the newest value. `detach` and
/// `report` as for [`Watch`]. Cheap to clone.
pub struct NativeWatch<T, E = ChordError>(Arc<Lifecycle<T, E>>);

impl<T, E> Clone for NativeWatch<T, E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T, E> NativeWatch<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    /// No replacement: an overflow delivers the newest value.
    pub fn new(value: T, detach: impl FnOnce() + Send + 'static, report: impl Fn(&E) + Send + Sync + 'static) -> Self {
        Self(Lifecycle::new(value, Box::new(detach), Arc::new(report), None, no_ops::<T>))
    }

    pub fn value(&self) -> T {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(T, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |value, _, context| Box::pin(listener(value, context))))
    }

    pub fn stop(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E> {
        self.0.terminate(WatchEnd::Stopped);
        self.0.closed()
    }

    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E> {
        self.0.closed()
    }

    pub fn observe_cancellation(&self, signal: &AbortSignal) -> Result<(), E> {
        self.0.observe_cancellation(signal)
    }

    pub fn cancel(&self) {
        self.0.terminate(WatchEnd::Cancelled);
    }

    pub fn end(&self, failure: Option<E>) {
        self.0.terminate(failure.map_or(WatchEnd::Ended, WatchEnd::Failed));
    }

    /// Queue `value`, the source's new value.
    pub fn advance(&self, value: T, context: Context) {
        self.0.enqueue(value, Vec::new(), context);
    }
}

impl<T, E> NativeWatchHandle<T, E> for NativeWatch<T, E>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    fn value(&self) -> T {
        NativeWatch::value(self)
    }

    fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(T, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        NativeWatch::start(self, listener)
    }

    fn stop(&self) -> BoxFuture<WatchEnd<E>> {
        Box::pin(NativeWatch::stop(self))
    }

    fn closed(&self) -> BoxFuture<WatchEnd<E>> {
        Box::pin(NativeWatch::closed(self))
    }
}
