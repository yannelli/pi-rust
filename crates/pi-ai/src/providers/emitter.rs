//! Direct event emission for adapters whose block indices or event order come from the
//! wire, where `MessageBuilder` would reorder events.

use std::time::Instant;

use crate::stream::{AssistantMessageEvent, EventSender};
use crate::types::{AssistantMessage, ContentBlock, Model, StopReason};

pub(crate) struct Emitter {
    sender: EventSender,
    pub message: AssistantMessage,
    started: Instant,
    finished: bool,
}

impl Emitter {
    pub fn new(model: &Model, sender: EventSender) -> Self {
        Self { sender, message: AssistantMessage::empty(model), started: Instant::now(), finished: false }
    }

    /// Push an event built from a snapshot of the current message.
    pub fn emit(&self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        self.sender.push(event(self.message.clone()));
    }

    pub fn start(&self) {
        self.emit(|partial| AssistantMessageEvent::Start { partial });
    }

    /// Append `block` and emit its start event; returns the content index.
    pub fn open(&mut self, block: ContentBlock) -> usize {
        self.message.content.push(block);
        let content_index = self.message.content.len() - 1;
        match &self.message.content[content_index] {
            ContentBlock::Thinking { .. } => {
                self.emit(|partial| AssistantMessageEvent::ThinkingStart { content_index, partial })
            }
            ContentBlock::ToolCall(_) => {
                self.emit(|partial| AssistantMessageEvent::ToolcallStart { content_index, partial })
            }
            _ => self.emit(|partial| AssistantMessageEvent::TextStart { content_index, partial }),
        }
        content_index
    }

    /// Emit the end event of the text or thinking block at `content_index`.
    pub fn close(&self, content_index: usize) {
        match self.message.content.get(content_index) {
            Some(ContentBlock::Text { text, .. }) => {
                let content = text.clone();
                self.emit(|partial| AssistantMessageEvent::TextEnd { content_index, content, partial });
            }
            Some(ContentBlock::Thinking { thinking, .. }) => {
                let content = thinking.clone();
                self.emit(|partial| AssistantMessageEvent::ThinkingEnd { content_index, content, partial });
            }
            _ => {}
        }
    }

    /// Emit a terminal event with the message stamped with `reason` and its duration.
    pub fn finish(mut self, reason: StopReason, error: Option<String>) {
        self.message.stop_reason = reason;
        if error.is_some() {
            self.message.error_message = error;
        }
        self.message.duration_ms = Some(self.started.elapsed().as_millis() as u64);
        self.finished = true;
        let message = self.message.clone();
        match reason {
            StopReason::Error | StopReason::Aborted => {
                self.sender.push(AssistantMessageEvent::Error { reason, error: message })
            }
            _ => self.sender.push(AssistantMessageEvent::Done { reason, message }),
        }
    }
}

impl Drop for Emitter {
    fn drop(&mut self) {
        if !self.finished {
            self.message.stop_reason = StopReason::Error;
            self.message.error_message = Some("Provider stopped before completing the response".into());
            let error = self.message.clone();
            self.sender.push(AssistantMessageEvent::Error { reason: StopReason::Error, error });
        }
    }
}
