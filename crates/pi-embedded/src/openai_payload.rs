//! Chat Completions request bodies and SSE frame decoding for the embedded provider.

use pi_ai::transcript::{current_system_prompt, current_tools, normalize_context};
use pi_ai::{ContentBlock, Context, Message, Model, UserContent};
use serde_json::{Map, Value, json};

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// Incremental `data:` frame decoder over decoded UTF-8 chunks.
#[derive(Default)]
pub(crate) struct SseDecoder {
    buffer: String,
    data: Vec<String>,
}

fn data_line(line: &str) -> Option<String> {
    let value = line.strip_prefix("data:")?;
    Some(value.strip_prefix(' ').unwrap_or(value).to_string())
}

impl SseDecoder {
    /// Append a chunk and return the frames it completed.
    pub fn push(&mut self, chunk: &str) -> anyhow::Result<Vec<String>> {
        self.buffer.push_str(chunk);
        if self.buffer.len() > MAX_FRAME_BYTES {
            anyhow::bail!("Provider SSE frame exceeds 4 MiB");
        }
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=newline).collect();
            let line = line[..line.len() - 1].strip_suffix('\r').unwrap_or(&line[..line.len() - 1]);
            if line.is_empty() {
                if !self.data.is_empty() {
                    frames.push(std::mem::take(&mut self.data).join("\n"));
                }
            } else if let Some(value) = data_line(line) {
                self.data.push(value);
            }
        }
        Ok(frames)
    }

    /// Frames left when the body ends without a trailing blank line.
    pub fn finish(mut self) -> Option<String> {
        if let Some(value) = data_line(&self.buffer) {
            self.data.push(value.strip_suffix('\r').unwrap_or(&value).to_string());
        }
        (!self.data.is_empty()).then(|| self.data.join("\n"))
    }
}

fn user_content(content: &UserContent) -> Value {
    match content {
        UserContent::Text(text) => Value::String(text.clone()),
        UserContent::Blocks(blocks) => Value::Array(
            blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text, .. } => Some(json!({"type": "text", "text": text})),
                    ContentBlock::Image { data, mime_type } => Some(
                        json!({"type": "image_url", "image_url": {"url": format!("data:{mime_type};base64,{data}")}}),
                    ),
                    _ => None,
                })
                .collect(),
        ),
    }
}

/// The streaming Chat Completions body for `context`, with the replayed system prompt and tools.
pub(crate) fn payload(model: &Model, context: &Context) -> Value {
    let transcript = normalize_context(context);
    let mut messages = Vec::new();
    let system = current_system_prompt(&transcript);
    if !system.is_empty() {
        messages.push(json!({"role": "system", "content": system}));
    }
    for message in &transcript {
        match message {
            Message::System(_) | Message::Custom(_) => {}
            Message::User(user) => messages.push(json!({"role": "user", "content": user_content(&user.content)})),
            Message::ToolResult(result) => {
                let content: Vec<String> = result
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text, .. } => Some(text.clone()),
                        ContentBlock::Image { mime_type, .. } => Some(format!("[Image {mime_type}]")),
                        _ => None,
                    })
                    .collect();
                messages
                    .push(json!({"role": "tool", "tool_call_id": result.tool_call_id, "content": content.join("\n")}));
            }
            Message::Assistant(assistant) => {
                let mut entry = Map::new();
                entry.insert("role".into(), json!("assistant"));
                entry.insert("content".into(), json!(assistant.text()));
                let calls: Vec<Value> = assistant
                    .tool_calls()
                    .map(|call| {
                        let arguments = Value::Object(call.arguments.clone()).to_string();
                        json!({"id": call.id, "type": "function", "function": {"name": call.name, "arguments": arguments}})
                    })
                    .collect();
                if !calls.is_empty() {
                    entry.insert("tool_calls".into(), Value::Array(calls));
                }
                messages.push(Value::Object(entry));
            }
        }
    }
    let mut body = json!({
        "model": model.id,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_completion_tokens": model.max_tokens,
    });
    let tools = current_tools(&transcript);
    if !tools.is_empty() {
        let tools: Vec<Value> = tools
            .iter()
            .map(|tool| {
                let function =
                    json!({"name": tool.name, "description": tool.description, "parameters": tool.parameters});
                json!({"type": "function", "function": function})
            })
            .collect();
        body["tools"] = Value::Array(tools);
    }
    body
}
