//! Port of upstream `test/telemetry.test.ts`: `pi.ai.request` and `pi.ai.poll` spans, events, status, and metrics
//! observed around `stream` and `stream_deferred` without changing the delivered events.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use pi_ai::{
    AI_TELEMETRY_SCHEMA, ApiProvider, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream,
    ContentBlock, Context, DeferredHandle, DeferredOptions, Model, RequestOptions, StopReason, StreamOptions,
    ThinkingLevel, ToolCall, Usage, event_channel,
};
use pi_telemetry::{
    InMemoryTelemetryContext, RecordedTelemetrySpan, SpanAttributes, SpanLink, SpanOptions, SpanOutcome, SpanStatus,
    TelemetryContext, TelemetryContextExt, TelemetryMetricKind, TelemetrySpan,
};

fn model(api: &str) -> Model {
    Model {
        id: "model".into(),
        name: "Model".into(),
        api: api.into(),
        provider: "test".into(),
        base_url: "https://example.test".into(),
        ..Model::default()
    }
}

fn handle(model: &Model) -> DeferredHandle {
    DeferredHandle {
        provider: model.provider.clone(),
        model_id: model.id.clone(),
        api: model.api.clone(),
        id: "deferred-secret".into(),
        expires_at: None,
        poll_after_ms: None,
        data: None,
    }
}

fn message(model: &Model, stop_reason: StopReason) -> AssistantMessage {
    let mut message = AssistantMessage::empty(model);
    let tool_call = ToolCall { id: "call-secret".into(), name: "lookup".into(), ..ToolCall::default() };
    message.content = vec![
        ContentBlock::text("text-secret"),
        ContentBlock::Thinking { thinking: "thinking-secret".into(), thinking_signature: None, redacted: false },
        ContentBlock::ToolCall(tool_call),
    ];
    message.usage =
        Usage { input: 11, output: 22, cache_read: 33, cache_write: 44, total_tokens: 110, ..Usage::default() };
    message.usage.cost.total = 1.0;
    message.response_id = Some("response".into());
    message.stop_reason = stop_reason;
    message
}

fn events(result: &AssistantMessage) -> Vec<AssistantMessageEvent> {
    let ContentBlock::ToolCall(tool_call) = result.content[2].clone() else { unreachable!() };
    let partial = result.clone();
    let terminal = match result.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            AssistantMessageEvent::Error { reason: result.stop_reason, error: result.clone() }
        }
        reason => AssistantMessageEvent::Done { reason, message: result.clone() },
    };
    vec![
        AssistantMessageEvent::Start { partial: partial.clone() },
        AssistantMessageEvent::TextDelta { content_index: 0, delta: "text-secret".into(), partial: partial.clone() },
        AssistantMessageEvent::TextEnd { content_index: 0, content: "text-secret".into(), partial: partial.clone() },
        AssistantMessageEvent::ThinkingEnd {
            content_index: 1,
            content: "thinking-secret".into(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolcallDelta {
            content_index: 2,
            delta: r#"{"value":"#.into(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolcallDelta {
            content_index: 2,
            delta: r#""argument-secret"}"#.into(),
            partial: partial.clone(),
        },
        AssistantMessageEvent::ToolcallEnd { content_index: 2, tool_call, partial },
        terminal,
    ]
}

fn replay(events: &[AssistantMessageEvent]) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    for event in events {
        sender.push(event.clone());
    }
    stream
}

/// Replays the same scripted events for every stream and deferred fetch.
struct Scripted(Vec<AssistantMessageEvent>);

impl ApiProvider for Scripted {
    fn stream(&self, _: &Model, _: &Context, _: StreamOptions) -> AssistantMessageEventStream {
        replay(&self.0)
    }

    fn fetch_deferred(&self, _: &Model, _: &DeferredHandle, _: DeferredOptions) -> AssistantMessageEventStream {
        replay(&self.0)
    }
}

fn setup(api: &str, stop_reason: StopReason) -> (Model, AssistantMessage, Vec<AssistantMessageEvent>) {
    let model = model(api);
    let result = message(&model, stop_reason);
    let events = events(&result);
    pi_ai::register_api_provider(api, Arc::new(Scripted(events.clone())));
    (model, result, events)
}

fn stream_options(telemetry: Arc<dyn TelemetrySpan>) -> StreamOptions {
    StreamOptions { api_key: Some("key".into()), telemetry: Some(telemetry), ..StreamOptions::default() }
}

fn deferred_options(telemetry: Arc<dyn TelemetrySpan>) -> DeferredOptions {
    let request =
        RequestOptions { api_key: Some("key".into()), telemetry: Some(telemetry), ..RequestOptions::default() };
    DeferredOptions { request, wait_ms: None }
}

async fn complete(model: Model, parent: Arc<dyn TelemetrySpan>) -> AssistantMessage {
    pi_ai::complete(&model, &Context::default(), stream_options(parent)).await
}

/// Run `request` under a `caller` span and return the recorded caller and request spans once both settle.
async fn traced<F, Fut, T>(
    telemetry: &InMemoryTelemetryContext,
    request: F,
) -> (T, RecordedTelemetrySpan, RecordedTelemetrySpan)
where
    F: FnOnce(Arc<dyn TelemetrySpan>) -> Fut,
    Fut: Future<Output = T>,
{
    let output = telemetry.start_infallible_span(SpanOptions::new("caller"), request).await;
    for _ in 0..200 {
        if telemetry.get_spans().iter().all(|span| span.settled) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let mut spans = telemetry.get_spans();
    assert_eq!(spans.len(), 2, "{spans:?}");
    assert!(spans.iter().all(|span| span.settled), "{spans:?}");
    let request = spans.remove(1);
    (output, spans.remove(0), request)
}

fn block(kind: &str, index: usize, chars: usize, tool: Option<&str>) -> (String, SpanAttributes) {
    let attributes =
        SpanAttributes::new().with("type", kind).with("index", index).with("chars", chars).with_optional("tool", tool);
    ("pi.ai.block".into(), attributes)
}

fn recorded_events(span: &RecordedTelemetrySpan) -> Vec<(String, SpanAttributes)> {
    span.events.iter().map(|event| (event.name.clone(), event.attributes.clone())).collect()
}

#[tokio::test]
async fn observes_stream_and_deferred_without_changing_events_or_results() {
    for poll in [false, true] {
        let telemetry = InMemoryTelemetryContext::new();
        let (model, result, expected) = setup(&format!("telemetry-observe-{poll}"), StopReason::Stop);
        let (received, caller, span) = traced(&telemetry, |parent| {
            let model = model.clone();
            async move {
                let stream = if poll {
                    pi_ai::stream_deferred(&model, &handle(&model), deferred_options(parent))
                } else {
                    let options = StreamOptions { reasoning: Some(ThinkingLevel::High), ..stream_options(parent) };
                    pi_ai::stream(&model, &Context::default(), options)
                };
                stream.collect::<Vec<_>>().await
            }
        })
        .await;
        assert_eq!(received, expected);
        assert_eq!(received.last().and_then(AssistantMessageEvent::final_message), Some(&result));
        assert_eq!(span.name, if poll { "pi.ai.poll" } else { "pi.ai.request" });
        assert_eq!(span.parent_id, Some(caller.id));
        let attributes = SpanAttributes::new()
            .with("provider", "test")
            .with("api", model.api.clone())
            .with("model", "model")
            .with_optional("reasoning", (!poll).then_some("high"))
            .with("stop", "stop")
            .with("usage.input", 11)
            .with("usage.output", 22)
            .with("usage.cache_read", 33)
            .with("usage.cache_write", 44)
            .with("cost.total", 1.0)
            .with("response.id", "response");
        assert_eq!(span.attributes, attributes);
        assert_eq!(span.status, SpanStatus::Ok);
        assert_eq!(
            recorded_events(&span),
            vec![
                ("pi.ai.first_chunk".into(), SpanAttributes::new()),
                block("text", 0, 11, None),
                block("thinking", 1, 15, None),
                block("tool_call", 2, 27, Some("lookup")),
            ]
        );
        let recorded = serde_json::to_string(&span).unwrap();
        for secret in ["text-secret", "thinking-secret", "argument-secret", "call-secret", "deferred-secret"] {
            assert!(!recorded.contains(secret), "{secret} leaked");
        }

        let measurements = telemetry.get_metrics();
        assert_eq!(measurements.len(), 7);
        let counters: Vec<f64> = measurements
            .iter()
            .filter(|measurement| measurement.kind == TelemetryMetricKind::Counter)
            .map(|measurement| measurement.value)
            .collect();
        assert_eq!(counters, [11.0, 22.0, 33.0, 44.0, 1.0]);
        for measurement in &measurements {
            let keys: Vec<&str> = measurement.attributes.iter().map(|(key, _)| key.as_str()).collect();
            let expected: &[&str] = match measurement.name.as_str() {
                "pi.ai.tokens" => &["kind", "model", "provider"],
                "pi.ai.request.duration" => &["model", "provider", "stop"],
                _ => &["model", "provider"],
            };
            assert_eq!(keys, expected, "{}", measurement.name);
            if measurement.kind == TelemetryMetricKind::Histogram {
                assert_eq!(measurement.context.as_ref(), Some(&span.context));
            }
        }
        let names: Vec<&str> = measurements.iter().map(|measurement| measurement.name.as_str()).collect();
        assert_eq!(names[..2], ["pi.ai.first_chunk.duration", "pi.ai.request.duration"]);
    }
}

#[tokio::test]
async fn reports_bounded_error_status_and_keeps_aborted_ok() {
    let cases = [
        (StopReason::Error, Some(format!("{}discarded-tail", "x".repeat(500))), Some("x".repeat(500))),
        (StopReason::Error, None, Some(String::new())),
        (StopReason::Aborted, Some("error-secret".into()), None),
    ];
    for (index, (reason, error, expected)) in cases.into_iter().enumerate() {
        let telemetry = InMemoryTelemetryContext::new();
        let model = model(&format!("telemetry-status-{index}"));
        let mut result = message(&model, reason);
        result.error_message = error;
        pi_ai::register_api_provider(model.api.clone(), Arc::new(Scripted(events(&result))));
        let (received, _, span) = traced(&telemetry, |parent| complete(model.clone(), parent)).await;
        assert_eq!(received, result);
        let status = match expected {
            Some(message) => SpanStatus::error("provider_error", message),
            None => SpanStatus::Ok,
        };
        assert_eq!(span.status, status);
        let recorded = serde_json::to_string(&(&span.attributes, &span.events)).unwrap();
        assert!(!recorded.contains("xxx") && !recorded.contains("error-secret"), "{recorded}");
        assert!(!serde_json::to_string(&span).unwrap().contains("discarded-tail"));
    }
}

#[tokio::test]
async fn includes_setup_failures() {
    let telemetry = InMemoryTelemetryContext::new();
    let model = model("telemetry-unregistered");
    let (result, _, span) = traced(&telemetry, |parent| complete(model.clone(), parent)).await;
    assert_eq!(result.stop_reason, StopReason::Error);
    assert_eq!(recorded_events(&span), vec![("pi.ai.first_chunk".into(), SpanAttributes::new())]);
    let message = "No provider registered for API: telemetry-unregistered";
    assert_eq!(span.status, SpanStatus::error("provider_error", message));
    assert!(span.attributes.get("reasoning").is_none());
    assert!(!serde_json::to_string(&span.attributes).unwrap().contains("No provider"));
}

#[tokio::test]
async fn counts_interleaved_tool_deltas_independently() {
    let model = model("telemetry-interleaved");
    let result = message(&model, StopReason::Stop);
    let partial = AssistantMessage::empty(&model);
    let tool_call = ToolCall { name: "lookup".into(), ..ToolCall::default() };
    let delta = |content_index, delta: &str| AssistantMessageEvent::ToolcallDelta {
        content_index,
        delta: delta.into(),
        partial: partial.clone(),
    };
    let end = |content_index| AssistantMessageEvent::ToolcallEnd {
        content_index,
        tool_call: tool_call.clone(),
        partial: partial.clone(),
    };
    let script = vec![
        delta(2, "ab"),
        delta(3, "123"),
        delta(2, "c"),
        end(3),
        end(2),
        end(4),
        AssistantMessageEvent::Done { reason: StopReason::Stop, message: result },
    ];
    pi_ai::register_api_provider(model.api.clone(), Arc::new(Scripted(script)));
    let telemetry = InMemoryTelemetryContext::new();
    let (_, _, span) = traced(&telemetry, |parent| complete(model.clone(), parent)).await;
    assert_eq!(
        recorded_events(&span)[1..],
        [
            block("tool_call", 3, 3, Some("lookup")),
            block("tool_call", 2, 3, Some("lookup")),
            block("tool_call", 4, 0, Some("lookup")),
        ]
    );
}

#[tokio::test]
async fn records_usage_for_a_poll_only_when_final() {
    for (index, reason) in
        [StopReason::Pending, StopReason::Deferred, StopReason::Stop, StopReason::Error].into_iter().enumerate()
    {
        let telemetry = InMemoryTelemetryContext::new();
        let (model, _, _) = setup(&format!("telemetry-poll-{index}"), reason);
        traced(&telemetry, |parent| {
            let model = model.clone();
            async move { pi_ai::fetch_deferred(&model, &handle(&model), deferred_options(parent)).await }
        })
        .await;
        let metrics = telemetry.get_metrics();
        let counters = metrics.iter().filter(|metric| metric.kind == TelemetryMetricKind::Counter).count();
        let expected = if matches!(reason, StopReason::Pending | StopReason::Deferred) { 0 } else { 5 };
        assert_eq!(counters, expected, "{reason:?}");
    }
}

/// A span that forwards tracing but offers no metrics capability.
struct TracingOnly(Arc<dyn TelemetrySpan>);

impl TelemetryContext for TracingOnly {
    fn open_span(&self, options: SpanOptions) -> Arc<dyn TelemetrySpan> {
        self.0.open_span(options)
    }
}

impl TelemetrySpan for TracingOnly {
    fn add_link(&self, link: SpanLink) {
        self.0.add_link(link);
    }
    fn add_event(&self, name: &str, attributes: SpanAttributes) {
        self.0.add_event(name, attributes);
    }
    fn set_attributes(&self, attributes: SpanAttributes) {
        self.0.set_attributes(attributes);
    }
    fn set_status(&self, status: SpanStatus) {
        self.0.set_status(status);
    }
    fn end(&self, outcome: SpanOutcome) {
        self.0.end(outcome);
    }
}

#[tokio::test]
async fn supports_a_tracing_only_span_without_metrics() {
    let telemetry = InMemoryTelemetryContext::new();
    let (model, _, _) = setup("telemetry-tracing-only", StopReason::Stop);
    let (_, _, span) = traced(&telemetry, |parent| {
        let parent: Arc<dyn TelemetrySpan> = Arc::new(TracingOnly(parent));
        complete(model.clone(), parent)
    })
    .await;
    assert_eq!(span.name, "pi.ai.request");
    assert!(telemetry.get_metrics().is_empty());
}

#[tokio::test]
async fn defaults_to_no_telemetry() {
    let (model, result, _) = setup("telemetry-default", StopReason::Stop);
    let options = StreamOptions { api_key: Some("key".into()), ..StreamOptions::default() };
    assert_eq!(pi_ai::complete(&model, &Context::default(), options).await, result);
    let provider = &AI_TELEMETRY_SCHEMA.spans["pi.ai.request"].start_attributes["provider"];
    assert_eq!(provider.required, Some(true));
}
