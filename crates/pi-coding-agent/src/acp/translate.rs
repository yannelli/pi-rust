//! Translation between ACP content and pi messages and tool results (upstream
//! `translate/prompt.ts`, `translate/pi-messages.ts`, `translate/pi-tools.ts`).

use agent_client_protocol_schema::{ContentBlock, EmbeddedResourceResource};
use serde_json::Value;

const TITLE_CHARS: usize = 80;

/// Decoded byte length of base64 text, computed as Node's `Buffer.byteLength(s, 'base64')`.
pub fn base64_byte_length(text: &str) -> usize {
    let mut bytes = text.len();
    if text.ends_with('=') {
        bytes -= 1;
        if text[..bytes].ends_with('=') {
            bytes -= 1;
        }
    }
    (bytes * 3) >> 2
}

/// The prompt as pi text plus base64 images. Resources become text markers.
pub fn prompt_to_pi_message(blocks: &[ContentBlock]) -> (String, Vec<pi_ai::ContentBlock>) {
    let mut message = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => message.push_str(&text.text),
            ContentBlock::ResourceLink(link) => message.push_str(&format!("\n[Context] {}", link.uri)),
            ContentBlock::Image(image) => {
                images.push(pi_ai::ContentBlock::Image { data: image.data.clone(), mime_type: image.mime_type.clone() })
            }
            ContentBlock::Resource(resource) => match &resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    let mime = text.mime_type.as_deref().unwrap_or("text/plain");
                    message.push_str(&format!("\n[Embedded Context] {} ({mime})\n{}", text.uri, text.text));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    let mime = blob.mime_type.as_deref().unwrap_or("application/octet-stream");
                    let bytes = base64_byte_length(&blob.blob);
                    message.push_str(&format!("\n[Embedded Context] {} ({mime}, {bytes} bytes)", blob.uri));
                }
                #[allow(unreachable_patterns)]
                _ => {}
            },
            ContentBlock::Audio(audio) => {
                let bytes = base64_byte_length(&audio.data);
                message.push_str(&format!("\n[Audio] ({}, {bytes} bytes) not supported by pi-acp", audio.mime_type));
            }
            #[allow(unreachable_patterns)]
            _ => {}
        }
    }
    (message, images)
}

/// Text blocks joined, whitespace collapsed, and cut to 80 characters; `None` without text.
pub fn prompt_to_session_title(blocks: &[ContentBlock]) -> Option<String> {
    let texts: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    title_from_texts(&texts)
}

/// The title rule of [`prompt_to_session_title`] over plain strings.
pub fn title_from_texts(texts: &[&str]) -> Option<String> {
    let joined = texts.join(" ");
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then(|| normalized.chars().take(TITLE_CHARS).collect())
}

fn text_blocks(content: &Value) -> String {
    let Some(blocks) = content.as_array() else { return String::new() };
    blocks
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .concat()
}

/// User message content: a string, or its text blocks joined.
pub fn normalize_pi_message_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        other => text_blocks(other),
    }
}

/// Assistant content: only text blocks are replayed.
pub fn normalize_pi_assistant_text(content: &Value) -> String {
    text_blocks(content)
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn number_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).filter(|field| field.is_number()).and_then(|field| field.as_f64()).map(|number| number as i64)
}

/// Text of a tool result: the edit diff, text content, bash output, or the JSON.
pub fn tool_result_to_text(result: &Value) -> String {
    if result.is_null() || result == &Value::Bool(false) {
        return String::new();
    }
    let details = result.get("details").cloned().unwrap_or(Value::Null);
    if let Some(diff) = string_field(&details, "diff").filter(|diff| !diff.trim().is_empty()) {
        return diff.to_string();
    }
    let text = text_blocks(result.get("content").unwrap_or(&Value::Null));
    if !text.is_empty() {
        return text;
    }
    let stdout = string_field(&details, "stdout")
        .or_else(|| string_field(result, "stdout"))
        .or_else(|| string_field(&details, "output"))
        .or_else(|| string_field(result, "output"));
    let stderr = string_field(&details, "stderr").or_else(|| string_field(result, "stderr"));
    let exit_code = number_field(&details, "exitCode")
        .or_else(|| number_field(result, "exitCode"))
        .or_else(|| number_field(&details, "code"))
        .or_else(|| number_field(result, "code"));
    let stdout = stdout.filter(|text| !text.trim().is_empty());
    let stderr = stderr.filter(|text| !text.trim().is_empty());
    if stdout.is_some() || stderr.is_some() {
        let mut parts: Vec<String> = Vec::new();
        parts.extend(stdout.map(String::from));
        parts.extend(stderr.map(|text| format!("stderr:\n{text}")));
        parts.extend(exit_code.map(|code| format!("exit code: {code}")));
        return parts.join("\n\n").trim_end().to_string();
    }
    serde_json::to_string_pretty(result).unwrap_or_default()
}

/// The ACP tool kind for a pi tool name.
pub fn tool_kind(tool_name: &str) -> agent_client_protocol_schema::ToolKind {
    use agent_client_protocol_schema::ToolKind;
    match tool_name {
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        _ => ToolKind::Other,
    }
}
