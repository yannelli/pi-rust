//! Pi-owned request telemetry (upstream `telemetry.ts`): `pi.ai.request` and `pi.ai.poll` spans around
//! [`crate::stream`] and [`crate::stream_deferred`], observed on the forwarding path without provider changes.
//! Content is never recorded: blocks report their kind, index, and character count.

use std::sync::{Arc, LazyLock};
use std::time::Instant;

use futures::StreamExt;
use pi_telemetry::{
    NoopTelemetryMetrics, SchemaTelemetryCounter, SchemaTelemetryHistogram, SchemaTelemetrySpan, SpanAttributes,
    SpanStatus, TelemetryContext, TelemetrySchemaDefinition, TelemetrySpan, create_typed_metrics,
    create_typed_span_starter, define_telemetry_schema,
};
use serde_json::json;

use crate::stream::{AssistantMessageEvent, AssistantMessageEventStream, event_channel};
use crate::types::{AssistantMessage, Model, StopReason, ThinkingLevel};

const MAX_ERROR_MESSAGE_CHARS: usize = 500;

/// Pi-owned request metadata, bounded error status, events, and low-cardinality metrics.
pub static AI_TELEMETRY_SCHEMA: LazyLock<TelemetrySchemaDefinition> = LazyLock::new(|| {
    let provider = json!({"type": "string", "description": "Provider identifier.", "required": true});
    let model = json!({"type": "string", "description": "Requested model identifier.", "required": true});
    let stop_values = json!(["pending", "stop", "length", "toolUse", "error", "aborted", "deferred"]);
    let stop = json!({"type": "string", "description": "Normalized stop reason.", "values": stop_values});
    let request = json!({
        "description": "One logical model request, including authentication and provider setup.",
        "parents": {"kind": "any"},
        "startAttributes": {
            "provider": provider,
            "api": {"type": "string", "description": "API identifier.", "required": true},
            "model": model,
            "reasoning": {"type": "string", "description": "Scalar requested reasoning level.", "required": false},
        },
        "endAttributes": {
            "stop": stop,
            "usage.input": {"type": "number", "description": "Uncached input tokens."},
            "usage.output": {"type": "number", "description": "Output tokens."},
            "usage.cache_read": {"type": "number", "description": "Cache-read tokens."},
            "usage.cache_write": {"type": "number", "description": "Cache-write tokens."},
            "cost.total": {"type": "number", "description": "Total request cost in USD."},
            "response.id": {"type": "string", "description": "Provider response identifier.", "cardinality": "high"},
        },
        "events": {
            "pi.ai.first_chunk": {"description": "First normalized stream event delivered.", "attributes": {}},
            "pi.ai.block": {
                "description": "A completed content block; content is never recorded.",
                "attributes": {
                    "type": {"type": "string", "description": "Block kind.", "values": ["text", "thinking", "tool_call"], "required": true},
                    "index": {"type": "number", "description": "Content block index.", "required": true},
                    "chars": {"type": "number", "description": "UTF-16 character count.", "required": true},
                    "tool": {"type": "string", "description": "Tool name, for tool-call blocks only.", "required": false},
                },
            },
        },
        "status": {
            "default": "ok",
            "errorWhen": "Stop reason is error: provider_error with at most 500 error-message characters. Aborted stays ok.",
        },
    });
    let mut poll = request.clone();
    poll["description"] = json!("One deferred-response poll, not the deferred request's lifetime.");
    let mut required_stop = stop.clone();
    required_stop["required"] = json!(true);
    let token_kinds = json!(["input", "output", "cache_read", "cache_write"]);
    let schema = json!({
        "version": 1,
        "spans": {"pi.ai.request": request, "pi.ai.poll": poll},
        "metrics": {
            "pi.ai.request.duration": {
                "kind": "histogram",
                "description": "Logical request or poll duration.",
                "unit": "ms",
                "attributes": {"provider": provider, "model": model, "stop": required_stop},
            },
            "pi.ai.first_chunk.duration": {
                "kind": "histogram",
                "description": "Time to the first normalized stream event.",
                "unit": "ms",
                "attributes": {"provider": provider, "model": model},
            },
            "pi.ai.tokens": {
                "kind": "counter",
                "description": "Reported model token usage; pending and deferred polls are excluded.",
                "unit": "token",
                "attributes": {
                    "provider": provider,
                    "model": model,
                    "kind": {"type": "string", "description": "Token usage category.", "values": token_kinds, "required": true},
                },
            },
            "pi.ai.cost": {
                "kind": "counter",
                "description": "Reported model cost; pending and deferred polls are excluded.",
                "unit": "USD",
                "attributes": {"provider": provider, "model": model},
            },
        },
    });
    define_telemetry_schema(serde_json::from_value(schema).expect("the AI telemetry schema is well formed"))
});

struct Instruments {
    request_duration: SchemaTelemetryHistogram,
    first_chunk_duration: SchemaTelemetryHistogram,
    tokens: SchemaTelemetryCounter,
    cost: SchemaTelemetryCounter,
}

impl Instruments {
    fn new(parent: &dyn TelemetrySpan) -> Self {
        let metrics = parent.metrics().unwrap_or_else(|| Arc::new(NoopTelemetryMetrics));
        let schema = std::slice::from_ref(&*AI_TELEMETRY_SCHEMA);
        let metrics = create_typed_metrics(metrics, schema).expect("the AI schema has unique metric names");
        let declared = "the AI schema declares its metrics";
        Self {
            request_duration: metrics.histogram("pi.ai.request.duration").expect(declared),
            first_chunk_duration: metrics.histogram("pi.ai.first_chunk.duration").expect(declared),
            tokens: metrics.counter("pi.ai.tokens").expect(declared),
            cost: metrics.counter("pi.ai.cost").expect(declared),
        }
    }
}

struct Observer {
    span: SchemaTelemetrySpan,
    instruments: Instruments,
    attributes: SpanAttributes,
    started: Instant,
    poll: bool,
    first: bool,
    tool_chars: Vec<(usize, usize)>,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn stop_name(reason: StopReason) -> String {
    serde_json::to_value(reason).ok().and_then(|value| value.as_str().map(String::from)).unwrap_or_default()
}

impl Observer {
    fn elapsed_ms(&self) -> f64 {
        self.started.elapsed().as_secs_f64() * 1000.0
    }

    fn context(&self) -> Option<&dyn TelemetryContext> {
        Some(self.span.raw().as_ref())
    }

    fn block(&self, kind: &str, index: usize, chars: usize, tool: Option<&str>) {
        let attributes = SpanAttributes::new()
            .with("type", kind)
            .with("index", index)
            .with("chars", chars)
            .with_optional("tool", tool);
        let _ = self.span.add_event("pi.ai.block", attributes);
    }

    fn event(&mut self, event: &AssistantMessageEvent) {
        if self.first {
            self.first = false;
            let _ = self.span.add_event("pi.ai.first_chunk", SpanAttributes::new());
            let _ = self.instruments.first_chunk_duration.record(
                self.elapsed_ms(),
                self.attributes.clone(),
                self.context(),
            );
        }
        match event {
            AssistantMessageEvent::ToolcallDelta { content_index, delta, .. } => {
                match self.tool_chars.iter_mut().find(|(index, _)| index == content_index) {
                    Some((_, chars)) => *chars += utf16_len(delta),
                    None => self.tool_chars.push((*content_index, utf16_len(delta))),
                }
            }
            AssistantMessageEvent::TextEnd { content_index, content, .. } => {
                self.block("text", *content_index, utf16_len(content), None);
            }
            AssistantMessageEvent::ThinkingEnd { content_index, content, .. } => {
                self.block("thinking", *content_index, utf16_len(content), None);
            }
            AssistantMessageEvent::ToolcallEnd { content_index, tool_call, .. } => {
                let position = self.tool_chars.iter().position(|(index, _)| index == content_index);
                let chars = position.map_or(0, |position| self.tool_chars.swap_remove(position).1);
                self.block("tool_call", *content_index, chars, Some(&tool_call.name));
            }
            _ => {}
        }
    }

    fn result(&self, message: &AssistantMessage) {
        let usage = &message.usage;
        let stop = stop_name(message.stop_reason);
        let end = SpanAttributes::new()
            .with("stop", stop.clone())
            .with("usage.input", usage.input)
            .with("usage.output", usage.output)
            .with("usage.cache_read", usage.cache_read)
            .with("usage.cache_write", usage.cache_write)
            .with("cost.total", usage.cost.total)
            .with_optional("response.id", message.response_id.clone());
        let _ = self.span.set_attributes(end);
        if message.stop_reason == StopReason::Error {
            let error = message.error_message.as_deref().unwrap_or_default();
            let bounded: String = error.chars().take(MAX_ERROR_MESSAGE_CHARS).collect();
            self.span.set_status(SpanStatus::error("provider_error", bounded));
        }
        let duration = self.attributes.clone().with("stop", stop);
        let _ = self.instruments.request_duration.record(self.elapsed_ms(), duration, self.context());
        if self.poll && matches!(message.stop_reason, StopReason::Pending | StopReason::Deferred) {
            return;
        }
        let tokens = [
            ("input", usage.input),
            ("output", usage.output),
            ("cache_read", usage.cache_read),
            ("cache_write", usage.cache_write),
        ];
        for (kind, value) in tokens {
            let _ = self.instruments.tokens.add(value as f64, self.attributes.clone().with("kind", kind));
        }
        let _ = self.instruments.cost.add(usage.cost.total, self.attributes.clone());
    }
}

/// Upstream `observeModelRequest` with the `lazyStream` observer: forward `inner` unchanged inside a request or poll
/// span under `telemetry`. Without a parent span the stream is returned as is.
pub(crate) fn observe_model_request(
    model: &Model,
    telemetry: Option<Arc<dyn TelemetrySpan>>,
    reasoning: Option<ThinkingLevel>,
    poll: bool,
    mut inner: AssistantMessageEventStream,
) -> AssistantMessageEventStream {
    let Some(parent) = telemetry else { return inner };
    let started = Instant::now();
    let instruments = Instruments::new(parent.as_ref());
    let attributes = SpanAttributes::new().with("provider", model.provider.clone()).with("model", model.id.clone());
    let start = attributes
        .clone()
        .with("api", model.api.clone())
        .with_optional("reasoning", reasoning.map(ThinkingLevel::as_str));
    let starter = create_typed_span_starter(parent, std::slice::from_ref(&*AI_TELEMETRY_SCHEMA))
        .expect("the AI schema has unique span names");
    let name = if poll { "pi.ai.poll" } else { "pi.ai.request" };
    let (sender, outer) = event_channel();
    let forward = starter
        .start_span(name, start, move |span, _| async move {
            let mut observer =
                Observer { span, instruments, attributes, started, poll, first: true, tool_chars: Vec::new() };
            let mut result = None;
            while let Some(event) = inner.next().await {
                observer.event(&event);
                if let Some(message) = event.final_message() {
                    result = Some(message.clone());
                }
                sender.push(event);
            }
            if let Some(message) = &result {
                observer.result(message);
            }
            Ok::<(), std::convert::Infallible>(())
        })
        .expect("request start attributes match the AI schema");
    tokio::spawn(forward);
    outer
}
