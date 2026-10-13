//! Runner-independent conformance cases for telemetry adapters (upstream `pi-telemetry/testing`).
//! Upstream cases that feed unreadable JavaScript proxies have no Rust counterpart: typed
//! payloads cannot fail to read, so those passivity checks are omitted.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;

use crate::attributes::SpanAttributes;
use crate::memory::RecordedTelemetrySpan;
use crate::span::{SpanOptions, SpanStatus, TelemetryContext, TelemetryContextExt, TelemetrySpan};

/// A fresh adapter instance and normalized snapshot reader owned by one conformance case.
pub trait TelemetryAdapterFixture: Send + Sync {
    fn context(&self) -> Arc<dyn TelemetryContext>;
    fn get_spans(&self) -> BoxFuture<'_, Vec<RecordedTelemetrySpan>>;
}

/// Creates an isolated adapter fixture for one conformance case.
pub type TelemetryAdapterFixtureFactory =
    Arc<dyn Fn() -> BoxFuture<'static, Box<dyn TelemetryAdapterFixture>> + Send + Sync>;

type CaseFn = fn(Box<dyn TelemetryAdapterFixture>) -> BoxFuture<'static, Result<(), String>>;

/// One conformance case; register it with any test runner and await [`Self::run`].
pub struct TelemetryAdapterConformanceCase {
    pub group: &'static str,
    pub name: &'static str,
    factory: TelemetryAdapterFixtureFactory,
    test: CaseFn,
}

impl TelemetryAdapterConformanceCase {
    pub async fn run(&self) -> Result<(), String> {
        let fixture = (self.factory)().await;
        (self.test)(fixture).await
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition { Ok(()) } else { Err(message.into()) }
}

fn find<'a>(spans: &'a [RecordedTelemetrySpan], name: &str) -> Result<&'a RecordedTelemetrySpan, String> {
    spans.iter().find(|span| span.name == name).ok_or_else(|| format!("Expected recorded span {name}"))
}

fn check_eq<T: PartialEq + std::fmt::Debug>(actual: T, expected: T, what: &str) -> Result<(), String> {
    ensure(actual == expected, format!("{what}: expected {expected:?}, got {actual:?}"))
}

async fn admits_once(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let result = fixture.context().start_span(SpanOptions::new("success"), move |_span| {
        counter.fetch_add(1, Ordering::SeqCst);
        async { Ok::<_, String>(42) }
    });
    check_eq(calls.load(Ordering::SeqCst), 1, "admissions before await")?;
    check_eq(result.await, Ok(42), "result")?;
    let spans = fixture.get_spans().await;
    let span = find(&spans, "success")?;
    check_eq(&span.status, &SpanStatus::Ok, "status")?;
    ensure(span.settled, "span settled")
}

async fn preserves_errors(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let context = fixture.context();
    let sync = context.start_span(SpanOptions::new("sync-error"), |_| async { Err::<(), _>("sync") }).await;
    check_eq(sync, Err("sync"), "sync error")?;
    let asynchronous = context.start_span(SpanOptions::new("async-error"), |_| async {
        tokio_yield().await;
        Err::<(), _>("async")
    });
    check_eq(asynchronous.await, Err("async"), "async error")?;
    let spans = fixture.get_spans().await;
    for name in ["sync-error", "async-error"] {
        ensure(find(&spans, name)?.status.is_error(), format!("{name} has an error status"))?;
    }
    Ok(())
}

/// Yield once so the callback completes on a later poll.
async fn tokio_yield() {
    let mut yielded = false;
    futures::future::poll_fn(|context| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await;
}

async fn explicit_status(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let context = fixture.context();
    let expected = |message: &str| SpanStatus::error("Expected", message);
    let _ = context
        .start_span(SpanOptions::new("last-status"), |span| async move {
            span.set_status(SpanStatus::error("Expected", "first"));
            span.set_status(SpanStatus::Ok);
            Ok::<_, String>(())
        })
        .await;
    let thrown = context
        .start_span(SpanOptions::new("explicit-before-throw"), |span| async move {
            span.set_status(SpanStatus::Ok);
            Err::<(), _>("after explicit status")
        })
        .await;
    check_eq(thrown, Err("after explicit status"), "explicit before throw")?;
    let rejected = context
        .start_span(SpanOptions::new("explicit-before-rejection"), |span| async move {
            span.set_status(SpanStatus::error("Expected", "async failure"));
            tokio_yield().await;
            Err::<(), _>("after async explicit status")
        })
        .await;
    check_eq(rejected, Err("after async explicit status"), "explicit before rejection")?;
    let _ = context
        .start_infallible_span(SpanOptions::new("expected-failure"), |span| async move {
            span.set_status(SpanStatus::error("Expected", "returned failure"));
            false
        })
        .await;
    let spans = fixture.get_spans().await;
    check_eq(&find(&spans, "last-status")?.status, &SpanStatus::Ok, "last status")?;
    check_eq(&find(&spans, "explicit-before-throw")?.status, &SpanStatus::Ok, "explicit ok")?;
    check_eq(&find(&spans, "explicit-before-rejection")?.status, &expected("async failure"), "explicit error")?;
    check_eq(&find(&spans, "expected-failure")?.status, &expected("returned failure"), "returned failure")
}

async fn merges_attributes(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let start =
        SpanAttributes::new().with("start", "value").with("overwrite", "start").with_optional("ignored", None::<&str>);
    let _ = fixture
        .context()
        .start_infallible_span(SpanOptions::new("recording").with_attributes(start), |span| async move {
            span.set_attributes(SpanAttributes::new().with("count", 1).with("overwrite", "middle"));
            span.set_attributes(SpanAttributes::new().with_optional("count", None::<f64>).with("overwrite", "end"));
            span.add_event("first", SpanAttributes::new().with("index", 1).with_optional("ignored", None::<f64>));
            span.add_event("second", SpanAttributes::new().with("index", 2));
        })
        .await;
    let spans = fixture.get_spans().await;
    let span = find(&spans, "recording")?;
    let expected = SpanAttributes::new().with("start", "value").with("overwrite", "end").with("count", 1);
    check_eq(&span.attributes, &expected, "attributes")?;
    let events: Vec<(&str, &SpanAttributes)> = span.events.iter().map(|e| (e.name.as_str(), &e.attributes)).collect();
    let (first, second) = (SpanAttributes::new().with("index", 1), SpanAttributes::new().with("index", 2));
    check_eq(events, vec![("first", &first), ("second", &second)], "events")
}

async fn inert_after_settlement(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let initial = SpanAttributes::new().with("value", "initial");
    let settled: Arc<dyn TelemetrySpan> = fixture
        .context()
        .start_infallible_span(SpanOptions::new("settled").with_attributes(initial.clone()), |span| async move { span })
        .await;
    settled.set_attributes(SpanAttributes::new().with("value", "late"));
    settled.add_event("late", SpanAttributes::new().with("value", true));
    settled.set_status(SpanStatus::Error { error: None });
    let admitted = Arc::new(AtomicUsize::new(0));
    let counter = admitted.clone();
    let child = settled.start_infallible_span(SpanOptions::new("late-child"), move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
        async { 7 }
    });
    check_eq(admitted.load(Ordering::SeqCst), 1, "late child admitted")?;
    check_eq(child.await, 7, "late child result")?;
    let spans = fixture.get_spans().await;
    check_eq(spans.len(), 1, "recorded spans")?;
    check_eq(&spans[0].attributes, &initial, "attributes")?;
    ensure(spans[0].events.is_empty(), "no late events")?;
    check_eq(&spans[0].status, &SpanStatus::Ok, "status")
}

async fn parentage(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let (release, gate) = futures::channel::oneshot::channel::<()>();
    let _ = fixture
        .context()
        .start_infallible_span(SpanOptions::new("parent"), |parent| async move {
            let first = parent.start_infallible_span(SpanOptions::new("first-child"), |_| async move {
                let _ = gate.await;
            });
            let second = parent.start_infallible_span(SpanOptions::new("second-child"), |_| async { "done" });
            let second = second.await;
            let _ = release.send(());
            first.await;
            second
        })
        .await;
    let spans = fixture.get_spans().await;
    let (parent, first, second) =
        (find(&spans, "parent")?, find(&spans, "first-child")?, find(&spans, "second-child")?);
    check_eq(parent.parent_id, None, "parent root")?;
    check_eq(first.parent_id, Some(parent.id), "first parent")?;
    check_eq(second.parent_id, Some(parent.id), "second parent")?;
    let order = (second.end_sequence, first.end_sequence, parent.end_sequence);
    let (Some(second_end), Some(first_end), Some(parent_end)) = order else {
        return Err("every span has an end sequence".into());
    };
    ensure(second_end < first_end && first_end < parent_end, "end order follows settlement")
}

async fn dropped_span_fails(fixture: Box<dyn TelemetryAdapterFixture>) -> Result<(), String> {
    let pending =
        fixture.context().start_infallible_span(SpanOptions::new("dropped"), |_| std::future::pending::<()>());
    drop(pending);
    let spans = fixture.get_spans().await;
    let span = find(&spans, "dropped")?;
    ensure(span.settled && span.status.is_error(), "a dropped span settles with an error status")
}

/// Creates the conformance cases for the callback telemetry adapter contract.
pub fn create_telemetry_adapter_conformance(
    factory: TelemetryAdapterFixtureFactory,
) -> Vec<TelemetryAdapterConformanceCase> {
    fn boxed<Fut: Future<Output = Result<(), String>> + Send + 'static>(
        f: impl Fn(Box<dyn TelemetryAdapterFixture>) -> Fut,
        fixture: Box<dyn TelemetryAdapterFixture>,
    ) -> BoxFuture<'static, Result<(), String>> {
        Box::pin(f(fixture))
    }
    let cases: [(&'static str, &'static str, CaseFn); 7] = [
        ("callback lifecycle", "admits once synchronously and preserves the result", |f| boxed(admits_once, f)),
        ("callback lifecycle", "preserves synchronous and asynchronous errors", |f| boxed(preserves_errors, f)),
        ("callback lifecycle", "fails a span dropped before it settles", |f| boxed(dropped_span_fails, f)),
        ("status", "uses last explicit status without automatic overwrite", |f| boxed(explicit_status, f)),
        ("recording", "merges attributes and records ordered events", |f| boxed(merges_attributes, f)),
        ("recording", "makes calls after settlement inert", |f| boxed(inert_after_settlement, f)),
        ("parentage", "records nested and concurrent child relationships", |f| boxed(parentage, f)),
    ];
    cases
        .into_iter()
        .map(|(group, name, test)| TelemetryAdapterConformanceCase { group, name, factory: factory.clone(), test })
        .collect()
}
