//! Assistant event protocol and the builder that providers use to emit it.

use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Instant;

use futures::Stream;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::json::{parse_streaming_json, parse_tool_call_arguments};
use crate::types::{AssistantMessage, ContentBlock, DeferredHandle, JsonObject, Model, StopReason, ToolCall};

/// Streaming events. A stream emits `Start` first and ends with exactly one
/// `Done` or `Error`; setup failures may emit `Error` without `Start`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AssistantMessageEvent {
    Start { partial: AssistantMessage },
    TextStart { content_index: usize, partial: AssistantMessage },
    TextDelta { content_index: usize, delta: String, partial: AssistantMessage },
    TextEnd { content_index: usize, content: String, partial: AssistantMessage },
    ThinkingStart { content_index: usize, partial: AssistantMessage },
    ThinkingDelta { content_index: usize, delta: String, partial: AssistantMessage },
    ThinkingEnd { content_index: usize, content: String, partial: AssistantMessage },
    ToolcallStart { content_index: usize, partial: AssistantMessage },
    ToolcallDelta { content_index: usize, delta: String, partial: AssistantMessage },
    ToolcallEnd { content_index: usize, tool_call: ToolCall, partial: AssistantMessage },
    Done { reason: StopReason, message: AssistantMessage },
    Error { reason: StopReason, error: AssistantMessage },
}

impl AssistantMessageEvent {
    /// The final message carried by `Done` or `Error`.
    pub fn final_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Receiving half of a provider response.
pub struct AssistantMessageEventStream {
    receiver: mpsc::UnboundedReceiver<AssistantMessageEvent>,
    finished: bool,
}

impl AssistantMessageEventStream {
    /// Drain the stream and return the final message. A stream that closes
    /// without a terminal event yields an `Error` message for `fallback`.
    pub async fn result(mut self, fallback: &Model) -> AssistantMessage {
        use futures::StreamExt;
        let mut last = None;
        while let Some(event) = self.next().await {
            if let Some(message) = event.final_message() {
                return message.clone();
            }
            last = Some(event);
        }
        let mut message = match last {
            Some(AssistantMessageEvent::Start { partial }) => partial,
            _ => AssistantMessage::empty(fallback),
        };
        message.stop_reason = StopReason::Error;
        message.error_message = Some("Provider stream ended without a final event".into());
        message
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let polled = self.receiver.poll_recv(context);
        if let Poll::Ready(Some(event)) = &polled
            && event.final_message().is_some()
        {
            self.finished = true;
        }
        polled
    }
}

/// Sending half of a provider response.
#[derive(Clone)]
pub struct EventSender {
    sender: mpsc::UnboundedSender<AssistantMessageEvent>,
}

impl EventSender {
    pub fn push(&self, event: AssistantMessageEvent) {
        let _ = self.sender.send(event);
    }
}

pub fn event_channel() -> (EventSender, AssistantMessageEventStream) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (EventSender { sender }, AssistantMessageEventStream { receiver, finished: false })
}

/// A stream that fails immediately, for request setup errors.
pub fn error_stream(model: &Model, reason: StopReason, error: impl Into<String>) -> AssistantMessageEventStream {
    let (sender, stream) = event_channel();
    let mut message = AssistantMessage::empty(model);
    message.stop_reason = reason;
    message.error_message = Some(error.into());
    sender.push(AssistantMessageEvent::Error { reason, error: message });
    stream
}

/// An assistant message diagnostic (`{type, timestamp, error?, details?}`), as upstream
/// `createAssistantMessageDiagnostic` builds it.
pub fn diagnostic(kind: &str, error: Option<&str>, details: Option<JsonObject>) -> Value {
    let mut value = serde_json::json!({"type": kind, "timestamp": crate::types::now_ms()});
    if let Some(message) = error {
        value["error"] = serde_json::json!({"message": message});
    }
    if let Some(details) = details {
        value["details"] = Value::Object(details);
    }
    value
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text(usize),
    Thinking(usize),
}

struct PendingToolCall {
    content_index: usize,
    json: String,
    ended: bool,
}

/// Builds the partial message and emits ordered events. Providers call the
/// delta methods as chunks arrive; open text and thinking blocks close automatically.
pub struct MessageBuilder {
    sender: EventSender,
    message: AssistantMessage,
    open: Option<OpenBlock>,
    tool_calls: Vec<PendingToolCall>,
    started: Instant,
    finished: bool,
}

impl MessageBuilder {
    pub fn new(model: &Model, sender: EventSender) -> Self {
        Self {
            sender,
            message: AssistantMessage::empty(model),
            open: None,
            tool_calls: Vec::new(),
            started: Instant::now(),
            finished: false,
        }
    }

    pub fn message(&self) -> &AssistantMessage {
        &self.message
    }

    pub fn message_mut(&mut self) -> &mut AssistantMessage {
        &mut self.message
    }

    pub fn start(&mut self) {
        self.sender.push(AssistantMessageEvent::Start { partial: self.message.clone() });
    }

    pub fn text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = match self.open {
            Some(OpenBlock::Text(index)) => index,
            _ => {
                self.close_open_block();
                self.message.content.push(ContentBlock::text(""));
                let index = self.message.content.len() - 1;
                self.open = Some(OpenBlock::Text(index));
                self.sender
                    .push(AssistantMessageEvent::TextStart { content_index: index, partial: self.message.clone() });
                index
            }
        };
        if let ContentBlock::Text { text, .. } = &mut self.message.content[index] {
            text.push_str(delta);
        }
        self.sender.push(AssistantMessageEvent::TextDelta {
            content_index: index,
            delta: delta.to_string(),
            partial: self.message.clone(),
        });
    }

    /// Set the signature of the open text block, if any.
    pub fn text_signature(&mut self, signature: String) {
        if let Some(OpenBlock::Text(index)) = self.open
            && let ContentBlock::Text { text_signature, .. } = &mut self.message.content[index]
        {
            *text_signature = Some(signature);
        }
    }

    pub fn thinking_delta(&mut self, delta: &str) {
        let index = self.open_thinking();
        if delta.is_empty() {
            return;
        }
        if let ContentBlock::Thinking { thinking, .. } = &mut self.message.content[index] {
            thinking.push_str(delta);
        }
        self.sender.push(AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta: delta.to_string(),
            partial: self.message.clone(),
        });
    }

    /// Attach a signature to the open thinking block, opening one if needed.
    pub fn thinking_signature(&mut self, signature: String, redacted: bool) {
        let index = self.open_thinking();
        if let ContentBlock::Thinking { thinking_signature, redacted: block_redacted, .. } =
            &mut self.message.content[index]
        {
            *thinking_signature = Some(signature);
            *block_redacted |= redacted;
        }
    }

    fn open_thinking(&mut self) -> usize {
        if let Some(OpenBlock::Thinking(index)) = self.open {
            return index;
        }
        self.close_open_block();
        self.message.content.push(ContentBlock::Thinking {
            thinking: String::new(),
            thinking_signature: None,
            redacted: false,
        });
        let index = self.message.content.len() - 1;
        self.open = Some(OpenBlock::Thinking(index));
        self.sender.push(AssistantMessageEvent::ThinkingStart { content_index: index, partial: self.message.clone() });
        index
    }

    /// Close the open text or thinking block, emitting its end event.
    pub fn close_open_block(&mut self) {
        match self.open.take() {
            Some(OpenBlock::Text(index)) => {
                let content = self.message.content[index].as_text().unwrap_or_default().to_string();
                self.sender.push(AssistantMessageEvent::TextEnd {
                    content_index: index,
                    content,
                    partial: self.message.clone(),
                });
            }
            Some(OpenBlock::Thinking(index)) => {
                let content = match &self.message.content[index] {
                    ContentBlock::Thinking { thinking, .. } => thinking.clone(),
                    _ => String::new(),
                };
                self.sender.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: index,
                    content,
                    partial: self.message.clone(),
                });
            }
            None => {}
        }
    }

    /// Start a tool call and return its handle for `tool_call_delta`.
    pub fn tool_call_start(&mut self, id: impl Into<String>, name: impl Into<String>) -> usize {
        self.close_open_block();
        self.message.content.push(ContentBlock::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: JsonObject::new(),
            thought_signature: None,
            ..Default::default()
        }));
        let content_index = self.message.content.len() - 1;
        self.tool_calls.push(PendingToolCall { content_index, json: String::new(), ended: false });
        self.sender.push(AssistantMessageEvent::ToolcallStart { content_index, partial: self.message.clone() });
        self.tool_calls.len() - 1
    }

    /// Mutable access to a started tool call, for ids or names that arrive later.
    pub fn tool_call_mut(&mut self, handle: usize) -> Option<&mut ToolCall> {
        let index = self.tool_calls.get(handle)?.content_index;
        match &mut self.message.content[index] {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        }
    }

    /// Append a raw JSON fragment of the tool arguments.
    pub fn tool_call_delta(&mut self, handle: usize, delta: &str) {
        let Some(pending) = self.tool_calls.get_mut(handle) else { return };
        pending.json.push_str(delta);
        let content_index = pending.content_index;
        if let Some(Value::Object(arguments)) = parse_streaming_json(&pending.json)
            && let ContentBlock::ToolCall(call) = &mut self.message.content[content_index]
        {
            call.arguments = arguments;
        }
        self.sender.push(AssistantMessageEvent::ToolcallDelta {
            content_index,
            delta: delta.to_string(),
            partial: self.message.clone(),
        });
    }

    /// Replace a tool call's raw arguments text. The call parses it strictly when it ends.
    pub fn tool_call_json(&mut self, handle: usize, json: &str) {
        let Some(pending) = self.tool_calls.get_mut(handle) else { return };
        pending.json = json.to_string();
        let arguments = match parse_streaming_json(json) {
            Some(Value::Object(arguments)) => arguments,
            _ => JsonObject::new(),
        };
        if let Some(call) = self.tool_call_mut(handle) {
            call.arguments = arguments;
        }
    }

    /// Replace a tool call's arguments with a complete object.
    pub fn tool_call_arguments(&mut self, handle: usize, arguments: JsonObject) {
        if let Some(pending) = self.tool_calls.get_mut(handle) {
            pending.json = Value::Object(arguments.clone()).to_string();
        }
        if let Some(call) = self.tool_call_mut(handle) {
            call.arguments = arguments;
        }
    }

    /// Finish one tool call: parse its arguments and emit `ToolcallEnd`. Calls that are still
    /// open when the message finishes end automatically.
    pub fn end_tool_call(&mut self, handle: usize) {
        let Some(pending) = self.tool_calls.get_mut(handle).filter(|pending| !pending.ended) else { return };
        pending.ended = true;
        let content_index = pending.content_index;
        // Arguments set through `tool_call_mut` have no JSON text and stay as they are.
        let parsed = (!pending.json.trim().is_empty()).then(|| parse_tool_call_arguments(&pending.json));
        let ContentBlock::ToolCall(call) = &mut self.message.content[content_index] else { return };
        if let Some((arguments, arguments_error)) = parsed {
            call.arguments = arguments;
            call.arguments_error = arguments_error;
        }
        let tool_call = call.clone();
        self.sender.push(AssistantMessageEvent::ToolcallEnd {
            content_index,
            tool_call,
            partial: self.message.clone(),
        });
    }

    fn end_tool_calls(&mut self) {
        for handle in 0..self.tool_calls.len() {
            self.end_tool_call(handle);
        }
    }

    /// Emit `Done` with reason `Deferred`: the provider continues the request later.
    pub fn defer(mut self, handle: DeferredHandle) {
        self.message.deferred = Some(handle);
        self.finish(StopReason::Deferred);
    }

    /// Append a redacted diagnostic to the message, see [`diagnostic`].
    pub fn add_diagnostic(&mut self, diagnostic: Value) {
        self.message.diagnostics.push(diagnostic);
    }

    /// Emit `Done`. `reason` must be `Stop`, `Length`, `ToolUse`, or `Deferred`; a `Stop`
    /// response that contains tool calls is reported as `ToolUse`.
    pub fn finish(mut self, reason: StopReason) {
        self.close_open_block();
        self.end_tool_calls();
        let reason = if reason == StopReason::Stop && self.message.tool_calls().next().is_some() {
            StopReason::ToolUse
        } else {
            reason
        };
        self.message.stop_reason = reason;
        self.message.duration_ms = Some(self.started.elapsed().as_millis() as u64);
        self.finished = true;
        self.sender.push(AssistantMessageEvent::Done { reason, message: self.message.clone() });
    }

    /// Emit `Error` with `reason` `Error` or `Aborted`.
    pub fn fail(mut self, reason: StopReason, error: impl Into<String>) {
        self.close_open_block();
        self.message.stop_reason = reason;
        self.message.error_message = Some(error.into());
        self.message.duration_ms = Some(self.started.elapsed().as_millis() as u64);
        self.finished = true;
        self.sender.push(AssistantMessageEvent::Error { reason, error: self.message.clone() });
    }
}

impl Drop for MessageBuilder {
    fn drop(&mut self) {
        if !self.finished {
            self.message.stop_reason = StopReason::Error;
            self.message.error_message = Some("Provider stopped before completing the response".into());
            self.sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error: self.message.clone() });
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;

    use super::*;
    use crate::types::{Model, ModelCost};

    fn model() -> Model {
        Model {
            id: "m".into(),
            name: "M".into(),
            api: "faux".into(),
            provider: "faux".into(),
            base_url: String::new(),
            reasoning: false,
            input: vec!["text".into()],
            cost: ModelCost::default(),
            context_window: 1000,
            max_tokens: 100,
            headers: Default::default(),
            compat: None,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn malformed_finished_tool_arguments_keep_no_partial_fields() {
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(&model(), sender);
        builder.start();
        let call = builder.tool_call_start("c1", "write");
        builder.tool_call_delta(call, r#"{"path":"a.txt","content": oops}"#);
        builder.finish(StopReason::Stop);
        let events: Vec<_> = stream.collect().await;
        let AssistantMessageEvent::Done { message, .. } = events.last().unwrap() else { panic!() };
        let call = message.tool_calls().next().unwrap();
        assert!(call.arguments.is_empty(), "{:?}", call.arguments);
        assert!(call.arguments_error.as_deref().unwrap().contains("Received arguments:\n{\"path\""));
    }

    #[tokio::test]
    async fn builder_emits_ordered_events_and_parses_tool_arguments() {
        let (sender, stream) = event_channel();
        let mut builder = MessageBuilder::new(&model(), sender);
        builder.start();
        builder.thinking_delta("plan");
        builder.text_delta("Hello");
        builder.text_delta(" world");
        let call = builder.tool_call_start("c1", "read");
        builder.tool_call_delta(call, "{\"path\":\"a");
        builder.tool_call_delta(call, ".txt\"}");
        builder.finish(StopReason::Stop);
        let events: Vec<_> = stream.collect().await;
        let kinds: Vec<_> = events
            .iter()
            .map(|event| serde_json::to_value(event).unwrap()["type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            kinds,
            [
                "start",
                "thinking_start",
                "thinking_delta",
                "thinking_end",
                "text_start",
                "text_delta",
                "text_delta",
                "text_end",
                "toolcall_start",
                "toolcall_delta",
                "toolcall_delta",
                "toolcall_end",
                "done"
            ]
        );
        let AssistantMessageEvent::Done { reason, message } = events.last().unwrap() else { panic!() };
        assert_eq!(*reason, StopReason::ToolUse);
        assert_eq!(message.text(), "Hello world");
        assert_eq!(message.tool_calls().next().unwrap().arguments["path"], "a.txt");
    }

    #[tokio::test]
    async fn dropped_builder_reports_an_error() {
        let (sender, stream) = event_channel();
        let builder = MessageBuilder::new(&model(), sender);
        drop(builder);
        let message = stream.result(&model()).await;
        assert_eq!(message.stop_reason, StopReason::Error);
    }
}
