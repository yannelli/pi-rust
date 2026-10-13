//! Codex stream events: Codex error and terminal shapes and service tier pricing around
//! the shared Responses event handler.

use serde_json::{Value, json};

use crate::providers::common::SseHandler;
use crate::providers::openai_responses::ResponsesStream;
use crate::registry::OnProviderStreamEventFn;
use crate::sse::SseEvent;
use crate::stream::MessageBuilder;
use crate::types::{Model, StopReason};

const CODEX_STATUSES: [&str; 6] = ["completed", "incomplete", "failed", "cancelled", "queued", "in_progress"];

/// Errors keep their class: API and protocol errors never fall back from WebSocket to SSE.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CodexError {
    Api { message: String, code: Option<String> },
    Protocol(String),
    Transport(String),
}

impl CodexError {
    pub fn message(&self) -> &str {
        match self {
            Self::Api { message, .. } | Self::Protocol(message) | Self::Transport(message) => message,
        }
    }

    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    pub fn is_transport(&self) -> bool {
        matches!(self, Self::Transport(_))
    }
}

/// Wraps [`ResponsesStream`] with the Codex-specific event mapping.
pub(super) struct CodexEvents {
    inner: ResponsesStream,
    model: Model,
    on_event: Option<OnProviderStreamEventFn>,
    /// Whether `Start` was emitted; the WebSocket transport emits it on the first event.
    pub started: bool,
    /// Whether this attempt saw a response event, after which it cannot fall back.
    pub seen: bool,
}

impl CodexEvents {
    pub fn new(
        inner: ResponsesStream,
        model: &Model,
        on_event: Option<OnProviderStreamEventFn>,
        started: bool,
    ) -> Self {
        Self { inner, model: model.clone(), on_event, started, seen: false }
    }

    /// Handle one raw JSON event. Returns true after the terminal response event.
    pub fn event(&mut self, builder: &mut MessageBuilder, text: &str, source: &str) -> Result<bool, CodexError> {
        let mut event: Value = serde_json::from_str(text)
            .map_err(|error| CodexError::Protocol(format!("Invalid Codex {source} JSON: {error}")))?;
        if let Some(hook) = &self.on_event {
            hook(&event, &self.model);
        }
        let Some(kind) = event.get("type").and_then(Value::as_str).map(str::to_string) else { return Ok(false) };
        match kind.as_str() {
            "error" => {
                let nested = |key: &str| event.get("error")?.get(key)?.as_str();
                let field =
                    |key: &str| event.get(key).and_then(Value::as_str).or_else(|| nested(key)).map(str::to_string);
                let (code, message) = (field("code"), field("message"));
                let detail = message.or_else(|| code.clone()).unwrap_or_else(|| event.to_string());
                return Err(CodexError::Api { message: format!("Codex error: {detail}"), code });
            }
            "response.failed" => {
                let error = event.pointer("/response/error");
                let field = |key: &str| error.and_then(|error| error.get(key)?.as_str()).map(str::to_string);
                let message = field("message").unwrap_or_else(|| "Codex response failed".into());
                return Err(CodexError::Api { message, code: field("code") });
            }
            _ => {}
        }
        self.seen = true;
        if !self.started {
            self.started = true;
            builder.start();
        }
        if matches!(kind.as_str(), "response.done" | "response.completed" | "response.incomplete") {
            self.terminal_event(builder, &mut event)?;
            return Ok(true);
        }
        self.feed(builder, text.to_string())?;
        Ok(false)
    }

    fn feed(&mut self, builder: &mut MessageBuilder, data: String) -> Result<(), CodexError> {
        self.inner.event(builder, SseEvent { event: None, data }).map(|_| ()).map_err(CodexError::Transport)
    }

    /// Codex terminal events become `response.completed` with a known status or none.
    fn terminal_event(&mut self, builder: &mut MessageBuilder, event: &mut Value) -> Result<(), CodexError> {
        let mut service_tier = None;
        if let Some(response) = event.get_mut("response").and_then(Value::as_object_mut) {
            if let Some(end_turn) = response.get("end_turn").and_then(Value::as_bool) {
                builder.message_mut().end_turn = Some(end_turn);
            }
            let status =
                response.get("status").and_then(Value::as_str).filter(|status| CODEX_STATUSES.contains(status));
            match status.map(str::to_string) {
                Some(status) => response.insert("status".into(), json!(status)),
                None => response.remove("status"),
            };
            service_tier = response.get("service_tier").and_then(Value::as_str).map(str::to_string);
        }
        event["type"] = json!("response.completed");
        self.feed(builder, event.to_string())?;
        let multiplier = match service_tier.as_deref() {
            Some("flex") => 0.5,
            Some("priority") if self.model.id == "gpt-5.5" => 2.5,
            Some("priority") => 2.0,
            _ => 1.0,
        };
        if multiplier != 1.0 {
            let cost = &mut builder.message_mut().usage.cost;
            cost.input *= multiplier;
            cost.output *= multiplier;
            cost.cache_read *= multiplier;
            cost.cache_write *= multiplier;
            cost.total = cost.input + cost.output + cost.cache_read + cost.cache_write;
        }
        Ok(())
    }

    /// The stop reason once the stream ends.
    pub fn end(&mut self, builder: &mut MessageBuilder) -> Result<StopReason, CodexError> {
        self.inner.end(builder).map_err(CodexError::Transport)
    }
}
