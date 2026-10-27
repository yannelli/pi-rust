//! Streamed tool calls: function calls with partial JSON arguments and grammar tool calls
//! (`custom`) whose raw input is re-encoded as an append-only JSON object.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::convert::GrammarInputs;
use crate::providers::constrained_sampling::{
    GrammarToolInputJsonBuffer, append_grammar_tool_input_json_delta, close_grammar_tool_input_json,
};
use crate::stream::{AssistantMessageEvent, EventSender, MessageBuilder};
use crate::streaming_json::ToolCallArgumentsStream;
use crate::types::{ContentBlock, JsonObject, ToolCall};

/// Grammar tool input: the argument property and the JSON re-encoding state.
struct CustomInput {
    property: String,
    buffer: GrammarToolInputJsonBuffer,
}

/// The input of a streaming tool call: JSON arguments, or the raw input of a grammar tool.
enum ToolInput {
    Json(Box<ToolCallArgumentsStream>),
    Custom(CustomInput),
}

/// A tool call being streamed: where it lives in the content and its input.
struct StreamingTool {
    content_index: usize,
    stream_index: Option<u64>,
    input: ToolInput,
}

pub(super) struct ToolCalls {
    grammar: GrammarInputs,
    tools: Vec<StreamingTool>,
    by_index: HashMap<u64, usize>,
    by_id: HashMap<String, usize>,
}

fn tool_call(builder: &mut MessageBuilder, index: usize) -> Option<&mut ToolCall> {
    match &mut builder.message_mut().content[index] {
        ContentBlock::ToolCall(call) => Some(call),
        _ => None,
    }
}

impl ToolCalls {
    pub fn new(grammar: GrammarInputs) -> Self {
        Self { grammar, tools: Vec::new(), by_index: HashMap::new(), by_id: HashMap::new() }
    }

    fn custom_for(&self, name: &str) -> CustomInput {
        let property = self.grammar.get(name).cloned().unwrap_or_else(|| "input".to_string());
        CustomInput { property, buffer: GrammarToolInputJsonBuffer::default() }
    }

    /// Apply one `tool_calls` delta. Calls match by stream index, then by id, so providers that
    /// change ids mid-stream still extend one call.
    pub fn delta(&mut self, builder: &mut MessageBuilder, sender: &EventSender, call: &Value) -> Result<(), String> {
        let stream_index = call.get("index").and_then(Value::as_u64);
        let id = call.get("id").and_then(Value::as_str).filter(|id| !id.is_empty());
        let name = [call.pointer("/function/name"), call.pointer("/custom/name")]
            .into_iter()
            .find_map(|name| name.and_then(Value::as_str))
            .filter(|name| !name.is_empty());
        let custom_only = call.get("custom").is_some_and(Value::is_object)
            && call.get("function").is_none_or(|function| function.is_null());
        let existing = stream_index
            .and_then(|index| self.by_index.get(&index))
            .or_else(|| id.and_then(|id| self.by_id.get(id)))
            .copied();
        let slot = match existing {
            Some(slot) => slot,
            None => self.start(builder, sender, stream_index, id, name, custom_only),
        };
        if let (Some(index), None) = (stream_index, self.tools[slot].stream_index) {
            self.tools[slot].stream_index = Some(index);
            self.by_index.insert(index, slot);
        }
        if let Some(id) = id {
            self.by_id.insert(id.to_string(), slot);
        }
        let content_index = self.tools[slot].content_index;
        let block_name = match tool_call(builder, content_index) {
            Some(block) => {
                if let (true, Some(id)) = (block.id.is_empty(), id) {
                    block.id = id.to_string();
                }
                if let (true, Some(name)) = (block.name.is_empty(), name) {
                    block.name = name.to_string();
                }
                block.name.clone()
            }
            None => String::new(),
        };
        if custom_only && !matches!(self.tools[slot].input, ToolInput::Custom(_)) {
            let custom = self.custom_for(&block_name);
            if let Some(block) = tool_call(builder, content_index) {
                block.arguments = JsonObject::from_iter([(custom.property.clone(), json!(""))]);
            }
            self.tools[slot].input = ToolInput::Custom(custom);
        }
        let delta = self.apply_input(builder, slot, call)?;
        let partial = builder.message().clone();
        sender.push(AssistantMessageEvent::ToolcallDelta { content_index, delta, partial });
        Ok(())
    }

    /// Append function arguments or grammar input; returns the delta to report.
    fn apply_input(&mut self, builder: &mut MessageBuilder, slot: usize, call: &Value) -> Result<String, String> {
        let tool = &mut self.tools[slot];
        let non_empty = |pointer: &str| call.pointer(pointer).and_then(Value::as_str).filter(|text| !text.is_empty());
        let Some(block) = tool_call(builder, tool.content_index) else { return Ok(String::new()) };
        match (&mut tool.input, non_empty("/function/arguments"), non_empty("/custom/input")) {
            (ToolInput::Json(arguments), Some(delta), _) => {
                block.arguments = arguments.append(delta).clone();
                Ok(delta.to_string())
            }
            (ToolInput::Custom(custom), _, Some(input)) => {
                let delta = append_grammar_tool_input_json_delta(&mut custom.buffer, &custom.property, input)?;
                block.arguments = JsonObject::from_iter([(custom.property.clone(), json!(custom.buffer.input))]);
                Ok(delta.unwrap_or_default())
            }
            _ => Ok(String::new()),
        }
    }

    fn start(
        &mut self,
        builder: &mut MessageBuilder,
        sender: &EventSender,
        stream_index: Option<u64>,
        id: Option<&str>,
        name: Option<&str>,
        custom_only: bool,
    ) -> usize {
        let custom = custom_only.then(|| self.custom_for(name.unwrap_or_default()));
        let arguments = match &custom {
            Some(custom) => JsonObject::from_iter([(custom.property.clone(), json!(""))]),
            None => JsonObject::new(),
        };
        let content = &mut builder.message_mut().content;
        content.push(ContentBlock::ToolCall(ToolCall {
            id: id.unwrap_or_default().to_string(),
            name: name.unwrap_or_default().to_string(),
            arguments,
            ..Default::default()
        }));
        let content_index = content.len() - 1;
        let slot = self.tools.len();
        let input = match custom {
            Some(custom) => ToolInput::Custom(custom),
            None => ToolInput::Json(Box::default()),
        };
        self.tools.push(StreamingTool { content_index, stream_index, input });
        if let Some(index) = stream_index {
            self.by_index.insert(index, slot);
        }
        if let Some(id) = id {
            self.by_id.insert(id.to_string(), slot);
        }
        sender.push(AssistantMessageEvent::ToolcallStart { content_index, partial: builder.message().clone() });
        slot
    }

    /// Finalize the call at `content_index`: parse its arguments strictly, or close its grammar input
    /// with a last delta, then emit its end event.
    pub fn finish(
        &mut self,
        builder: &mut MessageBuilder,
        sender: &EventSender,
        content_index: usize,
    ) -> Result<(), String> {
        let Some(tool) = self.tools.iter_mut().find(|tool| tool.content_index == content_index) else { return Ok(()) };
        let Some(block) = tool_call(builder, content_index) else { return Ok(()) };
        match &mut tool.input {
            ToolInput::Custom(custom) => {
                let delta = close_grammar_tool_input_json(&mut custom.buffer, &custom.property, None)?;
                block.arguments = JsonObject::from_iter([(custom.property.clone(), json!(custom.buffer.input))]);
                if let Some(delta) = delta {
                    let partial = builder.message().clone();
                    sender.push(AssistantMessageEvent::ToolcallDelta { content_index, delta, partial });
                }
            }
            ToolInput::Json(arguments) => (block.arguments, block.arguments_error) = arguments.finish(),
        }
        let partial = builder.message().clone();
        let ContentBlock::ToolCall(tool_call) = partial.content[content_index].clone() else { return Ok(()) };
        sender.push(AssistantMessageEvent::ToolcallEnd { content_index, tool_call, partial });
        Ok(())
    }
}
