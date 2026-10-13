//! Replay of assistant message frames (upstream `reduceAssistantMessageFrames`).

use std::collections::BTreeMap;

use crate::frames::{AssistantMessageFrame, FrameError, block_type, fail, parse_arguments};
use crate::types::{AssistantMessage, ContentBlock};

struct BlockState {
    kind: &'static str,
    ended: bool,
    /// Tool-call argument JSON received so far.
    json: String,
}

fn frame_name(frame: &AssistantMessageFrame) -> &'static str {
    use AssistantMessageFrame as F;
    match frame {
        F::Start { .. } => "start",
        F::TextStart { .. } => "text_start",
        F::TextDelta { .. } => "text_delta",
        F::TextEnd { .. } => "text_end",
        F::ThinkingStart { .. } => "thinking_start",
        F::ThinkingDelta { .. } => "thinking_delta",
        F::ThinkingEnd { .. } => "thinking_end",
        F::ToolcallStart { .. } => "toolcall_start",
        F::ToolcallCheckpoint { .. } => "toolcall_checkpoint",
        F::ToolcallDelta { .. } => "toolcall_delta",
        F::ToolcallEnd { .. } => "toolcall_end",
    }
}

struct Reducer {
    message: AssistantMessage,
    states: BTreeMap<usize, BlockState>,
}

impl Reducer {
    fn append(&mut self, name: &str, index: usize, block: &ContentBlock, kind: &'static str) -> Result<(), FrameError> {
        let found = block_type(block);
        if found != kind {
            return fail(format!("{name} frame contains {found} content"));
        }
        let length = self.message.content.len();
        if index != length {
            let reason = if index < length { "already exists" } else { "would leave a gap" };
            return fail(format!("Cannot start assistant message block at index {index}: {reason}"));
        }
        self.message.content.push(block.clone());
        self.states.insert(index, BlockState { kind, ended: false, json: String::new() });
        Ok(())
    }

    fn active(
        &mut self,
        name: &str,
        index: usize,
        kind: &str,
    ) -> Result<(&mut ContentBlock, &mut BlockState), FrameError> {
        let (Some(state), Some(block)) = (self.states.get_mut(&index), self.message.content.get_mut(index)) else {
            return fail(format!("{name} frame has no started block at index {index}"));
        };
        if state.kind != kind || block_type(block) != kind {
            return fail(format!("{name} frame expected {kind} block at index {index}, found {}", block_type(block)));
        }
        if state.ended {
            return fail(format!("{name} frame follows the end of block at index {index}"));
        }
        Ok((block, state))
    }

    fn apply(&mut self, frame: &AssistantMessageFrame) -> Result<(), FrameError> {
        use AssistantMessageFrame as F;
        let name = frame_name(frame);
        match frame {
            F::Start { .. } => return fail("Assistant message frame sequence contains more than one start frame"),
            F::TextStart { content_index, content } => self.append(name, *content_index, content, "text")?,
            F::ThinkingStart { content_index, content } => self.append(name, *content_index, content, "thinking")?,
            F::ToolcallStart { content_index, tool_call } => {
                self.append(name, *content_index, tool_call, "toolCall")?
            }
            F::TextDelta { content_index, delta } => {
                if let (ContentBlock::Text { text, .. }, _) = self.active(name, *content_index, "text")? {
                    text.push_str(delta);
                }
            }
            F::ThinkingDelta { content_index, delta } => {
                if let (ContentBlock::Thinking { thinking, .. }, _) = self.active(name, *content_index, "thinking")? {
                    thinking.push_str(delta);
                }
            }
            F::TextEnd { content_index, content, text_signature } => {
                let (block, state) = self.active(name, *content_index, "text")?;
                *block = ContentBlock::Text { text: content.clone(), text_signature: text_signature.clone() };
                state.ended = true;
            }
            F::ThinkingEnd { content_index, content, thinking_signature, redacted } => {
                let (block, state) = self.active(name, *content_index, "thinking")?;
                *block = ContentBlock::Thinking {
                    thinking: content.clone(),
                    thinking_signature: thinking_signature.clone(),
                    redacted: redacted.unwrap_or(false),
                };
                state.ended = true;
            }
            F::ToolcallCheckpoint { content_index, json } => {
                let (block, state) = self.active(name, *content_index, "toolCall")?;
                state.json = json.clone();
                if let ContentBlock::ToolCall(call) = block {
                    call.arguments = parse_arguments(json);
                }
            }
            F::ToolcallDelta { content_index, delta } => {
                let (_, state) = self.active(name, *content_index, "toolCall")?;
                state.json.push_str(delta);
            }
            F::ToolcallEnd { content_index, id, name: tool_name, arguments, thought_signature, namespace } => {
                let (block, state) = self.active(name, *content_index, "toolCall")?;
                if let ContentBlock::ToolCall(call) = block {
                    call.id = id.clone();
                    call.name = tool_name.clone();
                    call.arguments = arguments.clone();
                    call.thought_signature = thought_signature.clone();
                    call.namespace = namespace.clone();
                }
                state.ended = true;
            }
        }
        Ok(())
    }
}

/// Replay frames into the message they describe, without changing them. `Ok(None)` when the
/// frames hold no start frame. Unfinished tool calls get the arguments parsed from their JSON.
pub fn reduce_assistant_message_frames<'a>(
    frames: impl IntoIterator<Item = &'a AssistantMessageFrame>,
) -> Result<Option<AssistantMessage>, FrameError> {
    let mut reducer: Option<Reducer> = None;
    let mut frame_before_start: Option<&'static str> = None;
    for frame in frames {
        match (&mut reducer, frame) {
            (None, AssistantMessageFrame::Start { partial }) => {
                if let Some(before) = frame_before_start {
                    return fail(format!("{before} frame appears before the start frame"));
                }
                reducer = Some(Reducer { message: partial.as_ref().clone(), states: BTreeMap::new() });
            }
            (None, other) => {
                frame_before_start.get_or_insert(frame_name(other));
            }
            (Some(reducer), frame) => reducer.apply(frame)?,
        }
    }
    let Some(Reducer { mut message, states }) = reducer else {
        return Ok(None);
    };
    for (index, state) in states {
        if state.kind != "toolCall" || state.ended || state.json.is_empty() {
            continue;
        }
        if let Some(ContentBlock::ToolCall(call)) = message.content.get_mut(index) {
            call.arguments = parse_arguments(&state.json);
        }
    }
    Ok(Some(message))
}
