//! Compact, replayable assistant-message progress (upstream `utils/assistant-message-frame.ts`).
//! Terminal settlement is left out on purpose and must be persisted separately.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::json::parse_streaming_json;
use crate::stream::AssistantMessageEvent;
use crate::types::{AssistantMessage, ContentBlock, JsonObject, StopReason, ToolCall};

pub use crate::frames_reduce::reduce_assistant_message_frames;

/// One step of an assistant message stream, without the shared partial message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AssistantMessageFrame {
    Start {
        partial: Box<AssistantMessage>,
    },
    /// `content` is a text block.
    TextStart {
        content_index: usize,
        content: ContentBlock,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    /// `content` is a thinking block.
    ThinkingStart {
        content_index: usize,
        content: ContentBlock,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        redacted: Option<bool>,
    },
    /// `tool_call` is a tool-call block.
    ToolcallStart {
        content_index: usize,
        tool_call: ContentBlock,
    },
    /// The whole argument JSON so far, replacing earlier deltas.
    ToolcallCheckpoint {
        content_index: usize,
        json: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    },
    ToolcallEnd {
        content_index: usize,
        id: String,
        name: String,
        arguments: JsonObject,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
    },
}

/// A stream or frame sequence that breaks the frame rules.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FrameError(pub String);

pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T, FrameError> {
    Err(FrameError(message.into()))
}

/// The upstream `type` of a content block.
pub(crate) fn block_type(block: &ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { .. } => "text",
        ContentBlock::Thinking { .. } => "thinking",
        ContentBlock::Image { .. } => "image",
        ContentBlock::ToolCall(_) => "toolCall",
    }
}

pub(crate) fn parse_arguments(json: &str) -> JsonObject {
    match parse_streaming_json(json) {
        Some(Value::Object(map)) => map,
        _ => JsonObject::new(),
    }
}

enum EncoderBlock {
    /// Characters the start snapshot already held, and the delta characters seen since.
    Text {
        kind: &'static str,
        covered: usize,
        delta_chars: usize,
    },
    ToolCall {
        caught_up: bool,
        catchup_json: String,
        snapshot_arguments: String,
    },
}

impl EncoderBlock {
    fn kind(&self) -> &'static str {
        match self {
            Self::Text { kind, .. } => kind,
            Self::ToolCall { .. } => "toolCall",
        }
    }
}

fn serialized(arguments: &JsonObject) -> String {
    serde_json::to_string(arguments).unwrap_or_default()
}

/// Whether `current` extends `snapshot`: strings by prefix, arrays and objects member-wise.
fn is_json_prefix(snapshot: &Value, current: &Value) -> bool {
    match (snapshot, current) {
        (Value::String(snapshot), Value::String(current)) => current.starts_with(snapshot.as_str()),
        (Value::Array(snapshot), Value::Array(current)) => {
            snapshot.len() <= current.len() && snapshot.iter().zip(current).all(|(a, b)| is_json_prefix(a, b))
        }
        (Value::Object(snapshot), Value::Object(current)) => {
            snapshot.iter().all(|(key, value)| current.get(key).is_some_and(|current| is_json_prefix(value, current)))
        }
        (Value::String(_) | Value::Array(_) | Value::Object(_), _) => false,
        (snapshot, current) => snapshot == current,
    }
}

/// The start partial without content or settlement fields.
fn start_message(message: &AssistantMessage) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: message.api.clone(),
        provider: message.provider.clone(),
        model: message.model.clone(),
        response_model: message.response_model.clone(),
        response_id: message.response_id.clone(),
        provider_thinking_level: message.provider_thinking_level.clone(),
        thinking_level: None,
        diagnostics: message.diagnostics.clone(),
        usage: message.usage,
        stop_reason: StopReason::Pending,
        deferred: None,
        error_message: None,
        raw_stop_reason: None,
        end_turn: None,
        timestamp: message.timestamp,
        duration_ms: None,
    }
}

/// The block of `partial` the event points to, checked against the expected kind.
fn event_block<'a>(
    name: &str,
    partial: &'a AssistantMessage,
    index: usize,
    kind: &str,
) -> Result<&'a ContentBlock, FrameError> {
    let Some(block) = partial.content.get(index) else {
        return fail(format!("{name} event has no content block at index {index}"));
    };
    let found = block_type(block);
    if found != kind {
        return fail(format!("{name} event points to {found} block at index {index}"));
    }
    Ok(block)
}

/// Encodes one assistant stream into frames. Each event's `partial` may already hold content
/// of later events; per-block offsets keep deltas from being replayed twice.
#[derive(Default)]
pub struct AssistantMessageFrameEncoder {
    started: bool,
    terminal: bool,
    blocks: HashMap<usize, EncoderBlock>,
}

impl AssistantMessageFrameEncoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The frame of `event`, or `None` when it adds nothing (terminal events, covered deltas).
    pub fn encode(&mut self, event: &AssistantMessageEvent) -> Result<Option<AssistantMessageFrame>, FrameError> {
        use AssistantMessageEvent as E;
        use AssistantMessageFrame as F;
        let name = event_name(event);
        if self.terminal {
            return fail(format!("Assistant message event {name} follows a terminal event"));
        }
        match event {
            E::Start { partial } => {
                if self.started {
                    return fail("Assistant message stream contains more than one start event");
                }
                self.started = true;
                return Ok(Some(F::Start { partial: Box::new(start_message(partial)) }));
            }
            E::Done { .. } if !self.started => return fail("Assistant message done event appears before start"),
            E::Done { .. } | E::Error { .. } => {
                self.terminal = true;
                return Ok(None);
            }
            _ if !self.started => return fail(format!("Assistant message {name} event appears before start")),
            _ => {}
        }

        Ok(match event {
            E::TextStart { content_index: index, partial } => {
                let block = event_block(name, partial, *index, "text")?;
                let covered = block.as_text().unwrap_or_default().chars().count();
                self.start_block(*index, EncoderBlock::Text { kind: "text", covered, delta_chars: 0 })?;
                Some(F::TextStart { content_index: *index, content: block.clone() })
            }
            E::ThinkingStart { content_index: index, partial } => {
                let block = event_block(name, partial, *index, "thinking")?;
                let covered = match block {
                    ContentBlock::Thinking { thinking, .. } => thinking.chars().count(),
                    _ => 0,
                };
                self.start_block(*index, EncoderBlock::Text { kind: "thinking", covered, delta_chars: 0 })?;
                Some(F::ThinkingStart { content_index: *index, content: block.clone() })
            }
            E::TextDelta { content_index, delta, .. } => self.encode_text_delta(*content_index, delta, "text")?,
            E::ThinkingDelta { content_index, delta, .. } => {
                self.encode_text_delta(*content_index, delta, "thinking")?
            }
            E::TextEnd { content_index: index, content, partial } => {
                let block = event_block(name, partial, *index, "text")?;
                self.end_block(*index, "text")?;
                let text_signature = match block {
                    ContentBlock::Text { text_signature, .. } => text_signature.clone(),
                    _ => None,
                };
                Some(F::TextEnd { content_index: *index, content: content.clone(), text_signature })
            }
            E::ThinkingEnd { content_index: index, content, partial } => {
                let block = event_block(name, partial, *index, "thinking")?;
                self.end_block(*index, "thinking")?;
                let (thinking_signature, redacted) = match block {
                    ContentBlock::Thinking { thinking_signature, redacted, .. } => {
                        (thinking_signature.clone(), redacted.then_some(true))
                    }
                    _ => (None, None),
                };
                Some(F::ThinkingEnd { content_index: *index, content: content.clone(), thinking_signature, redacted })
            }
            E::ToolcallStart { content_index: index, partial } => {
                let block = event_block(name, partial, *index, "toolCall")?;
                let arguments = match block {
                    ContentBlock::ToolCall(call) => serialized(&call.arguments),
                    _ => String::new(),
                };
                let caught_up = arguments == serialized(&JsonObject::new());
                let snapshot_arguments = if caught_up { String::new() } else { arguments };
                let state = EncoderBlock::ToolCall { caught_up, catchup_json: String::new(), snapshot_arguments };
                self.start_block(*index, state)?;
                Some(F::ToolcallStart { content_index: *index, tool_call: block.clone() })
            }
            E::ToolcallDelta { content_index: index, delta, .. } => self.encode_tool_delta(*index, delta)?,
            E::ToolcallEnd { content_index: index, tool_call, partial } => {
                event_block(name, partial, *index, "toolCall")?;
                self.end_block(*index, "toolCall")?;
                let ToolCall { id, name, arguments, thought_signature, namespace } = tool_call.clone();
                Some(F::ToolcallEnd { content_index: *index, id, name, arguments, thought_signature, namespace })
            }
            E::Start { .. } | E::Done { .. } | E::Error { .. } => None,
        })
    }

    fn start_block(&mut self, index: usize, state: EncoderBlock) -> Result<(), FrameError> {
        if self.blocks.contains_key(&index) {
            return fail(format!("Assistant message block {index} starts more than once"));
        }
        self.blocks.insert(index, state);
        Ok(())
    }

    fn block(&mut self, index: usize, kind: &str) -> Result<&mut EncoderBlock, FrameError> {
        let Some(state) = self.blocks.get_mut(&index) else {
            return fail(format!("Assistant message {kind} block {index} has not started"));
        };
        if state.kind() != kind {
            return fail(format!("Assistant message block {index} is {}, not {kind}", state.kind()));
        }
        Ok(state)
    }

    fn end_block(&mut self, index: usize, kind: &str) -> Result<(), FrameError> {
        self.block(index, kind)?;
        self.blocks.remove(&index);
        Ok(())
    }

    fn encode_text_delta(
        &mut self,
        index: usize,
        delta: &str,
        kind: &str,
    ) -> Result<Option<AssistantMessageFrame>, FrameError> {
        let EncoderBlock::Text { covered, delta_chars, .. } = self.block(index, kind)? else {
            return fail("Unreachable text encoder state");
        };
        let start = *delta_chars;
        let length = delta.chars().count();
        *delta_chars += length;
        let skip = covered.saturating_sub(start);
        if skip >= length {
            return Ok(None);
        }
        let delta: String = delta.chars().skip(skip).collect();
        Ok(Some(match kind {
            "text" => AssistantMessageFrame::TextDelta { content_index: index, delta },
            _ => AssistantMessageFrame::ThinkingDelta { content_index: index, delta },
        }))
    }

    fn encode_tool_delta(&mut self, index: usize, delta: &str) -> Result<Option<AssistantMessageFrame>, FrameError> {
        let EncoderBlock::ToolCall { caught_up, catchup_json, snapshot_arguments } = self.block(index, "toolCall")?
        else {
            return fail("Unreachable tool-call encoder state");
        };
        if *caught_up {
            let frame = AssistantMessageFrame::ToolcallDelta { content_index: index, delta: delta.to_string() };
            return Ok((!delta.is_empty()).then_some(frame));
        }
        catchup_json.push_str(delta);
        let arguments = parse_arguments(catchup_json);
        if serialized(&arguments) != *snapshot_arguments {
            // Legacy grammar calls put the first input in toolcall_start, but their delta stream
            // still starts from empty input, so the parsed arguments may extend the snapshot.
            let snapshot = Value::Object(parse_arguments(snapshot_arguments));
            if !is_json_prefix(&snapshot, &Value::Object(arguments)) {
                return Ok(None);
            }
        }
        *caught_up = true;
        snapshot_arguments.clear();
        let json = std::mem::take(catchup_json);
        Ok((!json.is_empty()).then_some(AssistantMessageFrame::ToolcallCheckpoint { content_index: index, json }))
    }
}

/// The upstream `type` of an event, for error messages.
fn event_name(event: &AssistantMessageEvent) -> &'static str {
    use AssistantMessageEvent as E;
    match event {
        E::Start { .. } => "start",
        E::TextStart { .. } => "text_start",
        E::TextDelta { .. } => "text_delta",
        E::TextEnd { .. } => "text_end",
        E::ThinkingStart { .. } => "thinking_start",
        E::ThinkingDelta { .. } => "thinking_delta",
        E::ThinkingEnd { .. } => "thinking_end",
        E::ToolcallStart { .. } => "toolcall_start",
        E::ToolcallDelta { .. } => "toolcall_delta",
        E::ToolcallEnd { .. } => "toolcall_end",
        E::Done { .. } => "done",
        E::Error { .. } => "error",
    }
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
