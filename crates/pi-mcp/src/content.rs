//! Tool result content and its conversion to model content.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{JsonObject, ResourceContents};

/// A content block of a tool result. `annotations` and `_meta` stay in `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten)]
        extra: JsonObject,
    },
    Image {
        data: String,
        mime_type: String,
        #[serde(flatten)]
        extra: JsonObject,
    },
    Audio {
        data: String,
        mime_type: String,
        #[serde(flatten)]
        extra: JsonObject,
    },
    ResourceLink {
        uri: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<f64>,
        #[serde(flatten)]
        extra: JsonObject,
    },
    Resource {
        resource: ResourceContents,
        #[serde(flatten)]
        extra: JsonObject,
    },
    /// A block of a type this client does not know, kept as sent.
    #[serde(untagged)]
    Unknown(Value),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), extra: JsonObject::new() }
    }

    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image { data: data.into(), mime_type: mime_type.into(), extra: JsonObject::new() }
    }

    pub fn resource(resource: ResourceContents) -> Self {
        Self::Resource { resource, extra: JsonObject::new() }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(default, rename = "_meta", skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    #[serde(flatten)]
    pub extra: JsonObject,
}

impl CallToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(text)], ..Self::default() }
    }

    pub fn is_error(&self) -> bool {
        self.is_error == Some(true)
    }
}

fn block_to_llm_content(block: &ContentBlock) -> pi_ai::ContentBlock {
    match block {
        ContentBlock::Text { text, .. } => pi_ai::ContentBlock::text(text.clone()),
        ContentBlock::Image { data, mime_type, .. } => {
            pi_ai::ContentBlock::Image { data: data.clone(), mime_type: mime_type.clone() }
        }
        ContentBlock::Audio { mime_type, .. } => pi_ai::ContentBlock::text(format!("[audio {mime_type} omitted]")),
        ContentBlock::ResourceLink { uri, name, .. } => pi_ai::ContentBlock::text(format!("{name}: {uri}")),
        ContentBlock::Resource { resource, .. } => {
            if let Some(text) = &resource.text {
                return pi_ai::ContentBlock::text(text.clone());
            }
            let mime_type = resource.mime_type.as_deref();
            match (&resource.blob, mime_type) {
                (Some(blob), Some(mime)) if mime.starts_with("image/") => {
                    pi_ai::ContentBlock::Image { data: blob.clone(), mime_type: mime.into() }
                }
                _ => pi_ai::ContentBlock::text(format!(
                    "[binary resource {} ({}) omitted]",
                    resource.uri,
                    mime_type.unwrap_or("unknown type")
                )),
            }
        }
        ContentBlock::Unknown(value) => {
            let kind = value.get("type").map(|kind| kind.as_str().map(str::to_string).unwrap_or(kind.to_string()));
            pi_ai::ContentBlock::text(format!(
                "[unsupported MCP content {}]",
                kind.unwrap_or_else(|| "undefined".into())
            ))
        }
    }
}

/// Text and image content for a model. Text and images pass through, embedded text resources become
/// text, embedded image resources become images, and other blocks become a short text placeholder. A
/// result without blocks but with `structuredContent` becomes its JSON.
pub fn to_llm_content(content: &[ContentBlock], structured_content: Option<&Value>) -> Vec<pi_ai::ContentBlock> {
    let mut converted: Vec<pi_ai::ContentBlock> = content.iter().map(block_to_llm_content).collect();
    if converted.is_empty()
        && let Some(structured) = structured_content
    {
        converted.push(pi_ai::ContentBlock::text(pretty_json(structured)));
    }
    converted
}

/// JSON with two-space indentation, like `JSON.stringify(value, null, 2)`.
pub(crate) fn pretty_json(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn passes_text_and_images_through_and_replaces_other_blocks_with_text() {
        let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
            { "type": "text", "text": "hello", "annotations": { "priority": 1 } },
            { "type": "image", "data": "aW1n", "mimeType": "image/png", "_meta": { "x": 1 } },
            { "type": "audio", "data": "YXVk", "mimeType": "audio/wav" },
            { "type": "resource_link", "uri": "file:///a.txt", "name": "a.txt" },
            { "type": "resource", "resource": { "uri": "file:///b.txt", "text": "inline" } },
            { "type": "resource", "resource": { "uri": "file:///c.png", "mimeType": "image/png", "blob": "Yw==" } },
            { "type": "resource", "resource": { "uri": "file:///d.bin", "blob": "ZA==" } },
            { "type": "hologram", "data": 1 }
        ]))
        .unwrap();
        let texts: Vec<Value> = to_llm_content(&blocks, None).iter().map(|block| json!(block)).collect();
        assert_eq!(
            texts,
            vec![
                json!({ "type": "text", "text": "hello" }),
                json!({ "type": "image", "data": "aW1n", "mimeType": "image/png" }),
                json!({ "type": "text", "text": "[audio audio/wav omitted]" }),
                json!({ "type": "text", "text": "a.txt: file:///a.txt" }),
                json!({ "type": "text", "text": "inline" }),
                json!({ "type": "image", "data": "Yw==", "mimeType": "image/png" }),
                json!({ "type": "text", "text": "[binary resource file:///d.bin (unknown type) omitted]" }),
                json!({ "type": "text", "text": "[unsupported MCP content hologram]" }),
            ]
        );
        // Blocks serialize back as sent.
        assert_eq!(json!(blocks[0]), json!({ "type": "text", "text": "hello", "annotations": { "priority": 1 } }));
        assert_eq!(json!(blocks[7]), json!({ "type": "hologram", "data": 1 }));
    }

    #[test]
    fn falls_back_to_the_structured_content_as_json_when_there_are_no_blocks() {
        let structured = json!({ "n": 1 });
        assert_eq!(to_llm_content(&[], Some(&structured)), vec![pi_ai::ContentBlock::text("{\n  \"n\": 1\n}")]);
        assert_eq!(
            to_llm_content(&[ContentBlock::text("n=1")], Some(&structured)),
            vec![pi_ai::ContentBlock::text("n=1")]
        );
    }
}
