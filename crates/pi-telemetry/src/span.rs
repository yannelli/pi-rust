//! The callback span contract (upstream `TelemetryContext` and `TelemetrySpan`).

use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde::{Deserialize, Serialize};

use crate::attributes::SpanAttributes;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SpanOptions {
    pub name: String,
    #[serde(default, skip_serializing_if = "SpanAttributes::is_empty")]
    pub attributes: SpanAttributes,
}

impl SpanOptions {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), attributes: SpanAttributes::new() }
    }

    pub fn with_attributes(mut self, attributes: SpanAttributes) -> Self {
        self.attributes = attributes;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanErrorInfo {
    pub name: String,
    pub message: String,
}

/// `{"status":"ok"}` or `{"status":"error","error"?:{name,message}}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SpanStatus {
    #[default]
    Ok,
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<SpanErrorInfo>,
    },
}

impl SpanStatus {
    pub fn error(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error { error: Some(SpanErrorInfo { name: name.into(), message: message.into() }) }
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
}

/// How a span's callback settled. A failure sets an error status unless one was set explicitly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanOutcome {
    Ok,
    Failed(Option<SpanErrorInfo>),
}

/// An explicit parent for spans. Adapters must stay passive: they never fail or block the
/// instrumented operation, and a span opened under a settled span records nothing.
pub trait TelemetryContext: Send + Sync {
    /// Open a child span. [`TelemetryContextExt::start_span`] settles it through
    /// [`TelemetrySpan::end`]; adapters ignore calls after the first `end`.
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan>;
}

/// An open span. It is also the parent context for its children.
pub trait TelemetrySpan: TelemetryContext {
    fn add_event(&self, name: &str, attributes: SpanAttributes);
    fn set_attributes(&self, attributes: SpanAttributes);
    fn set_status(&self, status: SpanStatus);
    fn end(&self, outcome: SpanOutcome);
}

/// Ends the span exactly once; dropping it unsettled (panic or cancelled future) fails the span.
struct SpanGuard(Option<Arc<dyn TelemetrySpan>>);

impl SpanGuard {
    fn settle(&mut self, outcome: SpanOutcome) {
        if let Some(span) = self.0.take() {
            span.end(outcome);
        }
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        let cancelled = SpanErrorInfo { name: "Cancelled".into(), message: "Span dropped before it settled".into() };
        self.settle(SpanOutcome::Failed(Some(cancelled)));
    }
}

/// The future returned by the span starters; it settles the span when its callback completes.
pub struct SpanFuture<Fut: Future> {
    inner: Pin<Box<Fut>>,
    guard: SpanGuard,
    outcome: fn(&Fut::Output) -> SpanOutcome,
}

impl<Fut: Future> Future for SpanFuture<Fut> {
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match this.inner.as_mut().poll(context) {
            Poll::Ready(output) => {
                this.guard.settle((this.outcome)(&output));
                Poll::Ready(output)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn result_outcome<T, E: Display>(output: &Result<T, E>) -> SpanOutcome {
    match output {
        Ok(_) => SpanOutcome::Ok,
        Err(error) => SpanOutcome::Failed(Some(SpanErrorInfo { name: "Error".into(), message: error.to_string() })),
    }
}

fn ok_outcome<T>(_: &T) -> SpanOutcome {
    SpanOutcome::Ok
}

/// Callback span starters for every context, including `dyn TelemetrySpan` children.
pub trait TelemetryContextExt: TelemetryContext {
    /// Open a span, admit `callback` once immediately, and settle the span when its future
    /// completes. An `Err` output fails the span unless the callback set a status itself.
    fn start_span<F, Fut, T, E>(&self, options: SpanOptions, callback: F) -> SpanFuture<Fut>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: Display,
    {
        start(self, options, callback, result_outcome::<T, E>)
    }

    /// Like [`TelemetryContextExt::start_span`] for callbacks that cannot fail.
    fn start_infallible_span<F, Fut>(&self, options: SpanOptions, callback: F) -> SpanFuture<Fut>
    where
        F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut,
        Fut: Future,
    {
        start(self, options, callback, ok_outcome::<Fut::Output>)
    }
}

impl<C: TelemetryContext + ?Sized> TelemetryContextExt for C {}

fn start<C, F, Fut>(
    context: &C,
    options: SpanOptions,
    callback: F,
    outcome: fn(&Fut::Output) -> SpanOutcome,
) -> SpanFuture<Fut>
where
    C: TelemetryContext + ?Sized,
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut,
    Fut: Future,
{
    let span = context.open_span(options);
    let guard = SpanGuard(Some(span.clone()));
    let inner = Box::pin(callback(span));
    SpanFuture { inner, guard, outcome }
}
