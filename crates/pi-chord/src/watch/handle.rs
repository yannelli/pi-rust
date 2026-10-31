//! The watch handles sources create: `Watch` for JSON values with ops, `NativeWatch` for in-process values.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;

use super::lifecycle::{Lifecycle, PendingStore, Replace};
use super::{NativeWatchCall, NativeWatchHandle, WatchCall, WatchEnd, WatchHandle};
use crate::context::{AbortSignal, Context};
use crate::delta::{Op, PendingOps, PendingTake};
use crate::error::ChordError;
use crate::json::Json;
use crate::sync::BoxFuture;

/// Frames of a JSON watch waiting for its listener: their ops folded, the newest value and context, whether one was a
/// retirement, and a failure that came after the last value call.
pub(crate) struct JsonPending<E> {
    ops: PendingOps,
    /// The source's value call while it is the only frame pending: the call passes it on as it came.
    lone: Option<WatchCall<Json, E>>,
    newest_value: Option<Json>,
    /// The newest frame's context, while a call is pending; it also marks that one is.
    newest_context: Option<Context>,
    retired: bool,
    /// The newest frame was a failure: the next call is this.
    failure: Option<WatchCall<Json, E>>,
    /// A failure came after the last value call: the next value call replaces the root, and ops are not kept.
    replace: bool,
}

impl<E> Default for JsonPending<E> {
    fn default() -> Self {
        Self {
            ops: PendingOps::default(),
            lone: None,
            newest_value: None,
            newest_context: None,
            retired: false,
            failure: None,
            replace: false,
        }
    }
}

impl<E> JsonPending<E> {
    fn advance(&mut self, call: WatchCall<Json, E>, context: Context) -> bool {
        let (value, ops, retired) = match call {
            WatchCall::Value { value, ops, retired } => (value, ops, retired),
            failure => return self.fail(failure, context),
        };
        self.retired |= retired;
        if self.replace {
            self.failure = None;
        } else if self.newest_context.is_none() {
            self.lone = Some(WatchCall::Value { value: value.clone(), ops, retired });
        } else {
            if let Some(WatchCall::Value { ops, .. }) = self.lone.take() {
                self.ops.push(ops);
            }
            self.ops.push(ops);
        }
        self.newest_value = Some(value);
        self.newest_context = Some(context);
        true
    }

    /// Drop the pending frames and wait as one `Failed` call; the next value call replaces the root.
    fn fail(&mut self, call: WatchCall<Json, E>, context: Context) -> bool {
        self.ops.take();
        self.lone = None;
        self.newest_value = None;
        self.failure = Some(call);
        self.newest_context = Some(context);
        self.replace = true;
        true
    }

    /// The pending failure, or the value after one as a root replacement (no ops were kept since the failure).
    fn next_after_failure(&mut self, context: Context) -> Option<(WatchCall<Json, E>, Context)> {
        if let Some(failure) = self.failure.take() {
            return Some((failure, context));
        }
        let value = self.newest_value.take()?;
        let retired = std::mem::take(&mut self.retired);
        self.replace = false;
        Some((WatchCall::Value { value: value.clone(), ops: vec![Op::Replace(value)], retired }, context))
    }
}

impl<E: Send + 'static> PendingStore<WatchCall<Json, E>> for JsonPending<E> {
    fn next(&mut self) -> Option<(WatchCall<Json, E>, Context)> {
        let context = self.newest_context.take()?;
        if self.replace {
            return self.next_after_failure(context);
        }
        let value = self.newest_value.take()?;
        let retired = std::mem::take(&mut self.retired);
        // One frame never folds or overflows: its call is the listener's.
        if let Some(lone) = self.lone.take() {
            return Some((lone, context));
        }
        let ops = match self.ops.take() {
            PendingTake::Ops(ops) => ops,
            PendingTake::Replace => vec![Op::Replace(value.clone())],
        };
        Some((WatchCall::Value { value, ops, retired }, context))
    }

    fn has_pending(&self) -> bool {
        self.newest_context.is_some()
    }

    fn release(&mut self) {
        self.ops.take();
        self.lone = None;
        self.newest_value = None;
        self.newest_context = None;
        self.retired = false;
        self.failure = None;
        self.replace = false;
    }
}

/// Maximum values kept behind one unavailable native watch listener.
const MAX_PENDING_NATIVE_VALUES: usize = 100;

/// Values of a native watch waiting for its listener, in order, or one failure that dropped them.
pub(crate) struct NativePending<T, E> {
    values: VecDeque<(T, Context)>,
    replace: Option<Replace<T>>,
    /// A pending failure and its frame's context; nothing else is pending meanwhile.
    failure: Option<(E, Context)>,
}

impl<T: Send + Sync + 'static, E: Send + 'static> PendingStore<NativeWatchCall<T, E>> for NativePending<T, E> {
    fn next(&mut self) -> Option<(NativeWatchCall<T, E>, Context)> {
        if let Some((error, context)) = self.failure.take() {
            return Some((NativeWatchCall::Failed { error }, context));
        }
        let (value, context) = self.values.pop_front()?;
        Some((NativeWatchCall::Value { value }, context))
    }

    fn has_pending(&self) -> bool {
        self.failure.is_some() || !self.values.is_empty()
    }

    fn release(&mut self) {
        self.values.clear();
        self.replace = None;
        self.failure = None;
    }
}

fn call_value<E>(call: &WatchCall<Json, E>) -> Option<Json> {
    match call {
        WatchCall::Value { value, .. } => Some(value.clone()),
        WatchCall::Failed { .. } => None,
    }
}

fn native_value<T: Clone, E>(call: &NativeWatchCall<T, E>) -> Option<T> {
    match call {
        NativeWatchCall::Value { value } => Some(value.clone()),
        NativeWatchCall::Failed { .. } => None,
    }
}

type JsonWatchLifecycle<E> = Lifecycle<Json, WatchCall<Json, E>, E, JsonPending<E>>;

/// The watch handle of a JSON source. Frames that arrive before `start()` or while the listener runs wait in one
/// [`PendingOps`] queue; the next call gets the newest value, the folded ops (or `[["r", newest]]` after an overflow),
/// `retired` when any frame was a retirement, and the newest frame's context. Cheap to clone.
pub struct Watch<E = ChordError>(Arc<JsonWatchLifecycle<E>>);

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
        Self(Lifecycle::new(value, JsonPending::default(), Box::new(detach), Arc::new(report), call_value::<E>))
    }

    /// The acquisition value before `start()`, then the latest delivered value (a `Failed` call keeps it).
    pub fn value(&self) -> Json {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(WatchCall<Json, E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |call, context| Box::pin(listener(call, context))))
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

    /// End the watch `aborted` when `signal` aborts; installed once.
    pub fn observe_abort(&self, signal: &AbortSignal) -> Result<(), E> {
        self.0.observe_abort(signal)
    }

    /// End the watch `aborted`: its acquisition context was aborted.
    pub fn abort(&self) {
        self.0.terminate(WatchEnd::Aborted);
    }

    /// End the watch with `end` at once, dropping what is pending: the source closed (`Ended`), or failed for good
    /// (`Failed`). A failure the source may recover from is a `Failed` call instead (`fail()`).
    pub fn end(&self, end: WatchEnd<E>) {
        self.0.terminate(end);
    }

    /// The source sends nothing more: deliver what is pending, then end `ended` after the running call returns.
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }

    /// Queue the frame `call`. A value's ops turn the value before it into its value, and `retired` says the followed
    /// document was retired, which the next value call reports however many frames it folds; a failure is `fail()`.
    pub fn advance(&self, call: WatchCall<Json, E>, context: Context) {
        self.0.feed(|pending| pending.advance(call, context));
    }

    /// Queue a recoverable failure: it replaces the pending frames (`value()` keeps the last delivered value), and the
    /// first value after it arrives as `[["r", value]]`.
    pub fn fail(&self, error: E, context: Context) {
        self.0.feed(|pending| pending.fail(WatchCall::Failed { error }, context));
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
        F: Fn(WatchCall<Json, E>, Context) -> Fut + Send + Sync + 'static,
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

type NativeWatchLifecycle<T, E> = Lifecycle<T, NativeWatchCall<T, E>, E, NativePending<T, E>>;

/// The watch handle of a native source: every value in order, one call each; past 100 pending values they collapse to
/// one, `replace()` or by default the newest value. `detach` and `report` as for [`Watch`]. Cheap to clone.
pub struct NativeWatch<T, E = ChordError>(Arc<NativeWatchLifecycle<T, E>>);

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
        let pending = NativePending { values: VecDeque::new(), replace, failure: None };
        Self(Lifecycle::new(value, pending, Box::new(detach), Arc::new(report), native_value::<T, E>))
    }

    pub fn value(&self) -> T {
        self.0.value()
    }

    /// Install the sole serialized asynchronous listener. Delivery never runs inline.
    pub fn start<F, Fut>(&self, listener: F) -> Result<(), E>
    where
        F: Fn(NativeWatchCall<T, E>, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
    {
        self.0.start(Arc::new(move |call, context| Box::pin(listener(call, context))))
    }

    pub fn stop(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E> {
        self.0.terminate(WatchEnd::Stopped);
        self.0.closed()
    }

    pub fn closed(&self) -> impl Future<Output = WatchEnd<E>> + Send + use<T, E> {
        self.0.closed()
    }

    pub fn observe_abort(&self, signal: &AbortSignal) -> Result<(), E> {
        self.0.observe_abort(signal)
    }

    pub fn abort(&self) {
        self.0.terminate(WatchEnd::Aborted);
    }

    pub fn end(&self, end: WatchEnd<E>) {
        self.0.terminate(end);
    }

    /// The source sends nothing more: deliver what is pending, then end `ended` after the running call returns.
    pub fn end_after_delivery(&self) {
        self.0.end_after_delivery();
    }

    /// Queue `value`, the source's new value; it replaces a pending failure.
    pub fn advance(&self, value: T, context: Context) {
        self.0.feed(|pending| {
            pending.failure = None;
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

    /// Queue a recoverable failure: it replaces the pending values (`value()` keeps the last delivered value), and a
    /// value after it replaces it.
    pub fn fail(&self, error: E, context: Context) {
        self.0.feed(|pending| {
            pending.values.clear();
            pending.failure = Some((error, context));
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
        F: Fn(NativeWatchCall<T, E>, Context) -> Fut + Send + Sync + 'static,
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
