//! The watch handles sources create: `Watch` for JSON values with ops, `NativeWatch` for in-process values.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use super::lifecycle::{Lifecycle, PendingStore, Replace};
use super::{NativeWatchChange, NativeWatchHandle, WatchChange, WatchEnd, WatchHandle};
use crate::context::{AbortSignal, Context};
use crate::delta::{Op, PendingOps, PendingTake};
use crate::error::ChordError;
use crate::json::Json;
use crate::sync::BoxFuture;

/// Frames of a JSON watch waiting for its listener: their ops folded, the newest value and context, and whether one was a
/// retirement.
#[derive(Default)]
pub(crate) struct JsonPending {
    ops: PendingOps,
    /// The source's change while it is the only frame pending: the call passes it on as it came.
    lone: Option<WatchChange>,
    newest: Option<(Json, Context)>,
    retired: bool,
}

impl JsonPending {
    fn advance(&mut self, change: WatchChange, context: Context) -> bool {
        let first = self.newest.is_none();
        self.newest = Some((change.value.clone(), context));
        self.retired |= change.retired;
        if first {
            self.lone = Some(change);
        } else {
            if let Some(lone) = self.lone.take() {
                self.ops.push(lone.ops);
            }
            self.ops.push(change.ops);
        }
        true
    }
}

impl PendingStore<WatchChange> for JsonPending {
    fn next(&mut self) -> Option<(WatchChange, Context)> {
        let (value, context) = self.newest.take()?;
        let retired = std::mem::take(&mut self.retired);
        // One frame never folds or overflows: its change is the call's.
        if let Some(lone) = self.lone.take() {
            return Some((lone, context));
        }
        let ops = match self.ops.take() {
            PendingTake::Ops(ops) => ops,
            PendingTake::Replace => vec![Op::Replace(value.clone())],
        };
        Some((WatchChange { value, ops, retired }, context))
    }

    fn has_pending(&self) -> bool {
        self.newest.is_some()
    }

    fn release(&mut self) {
        self.ops.take();
        self.lone = None;
        self.newest = None;
        self.retired = false;
    }
}

/// Maximum values kept behind one unavailable native watch listener.
const MAX_PENDING_NATIVE_VALUES: usize = 100;

/// Values of a native watch waiting for its listener, in order.
pub(crate) struct NativePending<T> {
    values: VecDeque<(T, Context)>,
    replace: Option<Replace<T>>,
}

impl<T: Send + Sync + 'static> PendingStore<NativeWatchChange<T>> for NativePending<T> {
    fn next(&mut self) -> Option<(NativeWatchChange<T>, Context)> {
        let (value, context) = self.values.pop_front()?;
        Some((NativeWatchChange { value }, context))
    }

    fn has_pending(&self) -> bool {
        !self.values.is_empty()
    }

    fn release(&mut self) {
        self.values.clear();
        self.replace = None;
    }
}

fn change_value(change: &WatchChange) -> Json {
    change.value.clone()
}

fn native_value<T: Clone>(change: &NativeWatchChange<T>) -> T {
    change.value.clone()
}

/// The watch handle of a JSON source. Frames that arrive before `start()` or while the listener runs wait in one
/// [`PendingOps`] queue; the next call gets the newest value, the folded ops (or `[["r", newest]]` after an overflow),
/// `retired` when any frame was a retirement, and the newest frame's context. Cheap to clone.
pub struct Watch<E = ChordError>(Arc<Lifecycle<Json, WatchChange, E, JsonPending>>);

impl<E> Clone for Watch<E> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<E> Watch<E>
where
    E: Clone + Send + Sync + From<ChordError> + 'static,
{
    /// `detach` unsubscribes it from its source when it ends; `report` gets a listener's error, which also ends it.
    pub fn new(
        value: Json,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(&E) + Send + Sync + 'static,
    ) -> Self {
        Self(Lifecycle::new(value, JsonPending::default(), Box::new(detach), Arc::new(report), change_value))
    }

    /// The acquisition value before `start()`, then the latest delivered value.
    pub fn value(&self) -> Json {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(WatchChange, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |change, context| Box::pin(listener(change, context))))
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

    /// The source closed: end with `ended`, or with `failed` and the error when the source failed, at once, dropping
    /// what is pending.
    pub fn end(&self, failure: Option<E>) {
        self.0.terminate(failure.map_or(WatchEnd::Ended, WatchEnd::Failed));
    }

    /// The source sends nothing more: deliver what is pending, then end `ended` after the running call returns.
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }

    /// Queue the frame `change`: its ops turn the value before it into its value, and `retired` says the followed
    /// document was retired, which the next call reports however many frames it folds.
    pub fn advance(&self, change: WatchChange, context: Context) {
        self.0.feed(|pending| pending.advance(change, context));
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
        F: Fn(WatchChange, Context) -> Fut + Send + Sync + 'static,
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

/// The watch handle of a native source: every value in order, one call each; past 100 pending values they collapse to
/// one, `replace()` or by default the newest value. `detach` and `report` as for [`Watch`]. Cheap to clone.
pub struct NativeWatch<T, E = ChordError>(Arc<Lifecycle<T, NativeWatchChange<T>, E, NativePending<T>>>);

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
    pub fn new(
        value: T,
        detach: impl FnOnce() + Send + 'static,
        report: impl Fn(&E) + Send + Sync + 'static,
        replace: Option<Replace<T>>,
    ) -> Self {
        let pending = NativePending { values: VecDeque::new(), replace };
        Self(Lifecycle::new(value, pending, Box::new(detach), Arc::new(report), native_value::<T>))
    }

    pub fn value(&self) -> T {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(NativeWatchChange<T>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |change, context| Box::pin(listener(change, context))))
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

    /// The source sends nothing more: deliver what is pending, then end `ended` after the running call returns.
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }

    /// Queue `value`, the source's new value.
    pub fn advance(&self, value: T, context: Context) {
        self.0.feed(|pending| {
            if pending.values.len() >= MAX_PENDING_NATIVE_VALUES {
                pending.values.clear();
                let value = pending.replace.as_ref().map_or(value, |replace| replace());
                pending.values.push_back((value, context));
            } else {
                pending.values.push_back((value, context));
            }
            true
        });
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
        F: Fn(NativeWatchChange<T>, Context) -> Fut + Send + Sync + 'static,
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
