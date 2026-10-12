//! Message, content, usage, tool, and model types. JSON field names match the
//! TypeScript pi packages, so session files stay readable by both implementations.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type JsonObject = Map<String, Value>;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: JsonObject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
    },
    Image {
        data: String,
        mime_type: String,
    },
    ToolCall(ToolCall),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into(), text_signature: None }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Reasoning tokens, a subset of `output`, when the provider reports them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: Cost,
}

impl Usage {
    /// Fill `cost` from per-million-token model prices.
    pub fn apply_cost(&mut self, cost: &ModelCost) {
        let per_million = |tokens: u64, price: f64| tokens as f64 * price / 1_000_000.0;
        self.cost.input = per_million(self.input, cost.input);
        self.cost.output = per_million(self.output, cost.output);
        self.cost.cache_read = per_million(self.cache_read, cost.cache_read);
        self.cost.cache_write = per_million(self.cache_write, cost.cache_write);
        self.cost.total = self.cost.input + self.cost.output + self.cost.cache_read + self.cost.cache_write;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Pending,
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

impl UserContent {
    /// Text parts joined with newlines; images are omitted.
    pub fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Blocks(blocks) => blocks.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

impl AssistantMessage {
    /// An empty pending response for `model`.
    pub fn empty(model: &Model) -> Self {
        Self {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Pending,
            error_message: None,
            timestamp: now_ms(),
            duration_ms: None,
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// Text blocks joined without separators.
    pub fn text(&self) -> String {
        self.content.iter().filter_map(ContentBlock::as_text).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    pub is_error: bool,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User(UserMessage { content: UserContent::Text(text.into()), timestamp: now_ms() })
    }
}

/// A tool declaration sent to the model. `parameters` is a JSON Schema object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Request input for a provider.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 6] = [Self::Off, Self::Minimal, Self::Low, Self::Medium, Self::High, Self::Xhigh];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
        }
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|level| level.as_str() == value)
            .ok_or_else(|| format!("Unknown thinking level: {value}"))
    }
}

/// Prices in dollars per million tokens.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    /// Wire protocol, for example `openai-completions` or `anthropic-messages`.
    pub api: String,
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub reasoning: bool,
    /// Input modalities: `text`, `image`.
    #[serde(default = "default_input")]
    pub input: Vec<String>,
    #[serde(default)]
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Provider-specific compatibility settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
}

fn default_input() -> Vec<String> {
    vec!["text".into()]
}

impl Model {
    pub fn supports_images(&self) -> bool {
        self.input.iter().any(|input| input == "image")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_with_upstream_field_names() {
        let json = r#"{"role":"assistant","content":[{"type":"text","text":"hi"},{"type":"thinking","thinking":"t","thinkingSignature":"s"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"a"}}],"api":"anthropic-messages","provider":"anthropic","model":"m","usage":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":3,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"toolUse","timestamp":1}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        let Message::Assistant(assistant) = &message else { panic!("expected assistant") };
        assert_eq!(assistant.stop_reason, StopReason::ToolUse);
        assert_eq!(assistant.tool_calls().next().unwrap().name, "read");
        let encoded = serde_json::to_value(&message).unwrap();
        assert_eq!(encoded["content"][2]["type"], "toolCall");
        assert_eq!(encoded["content"][1]["thinkingSignature"], "s");
        assert_eq!(encoded["stopReason"], "toolUse");
        let user: Message = serde_json::from_str(r#"{"role":"user","content":"hello","timestamp":2}"#).unwrap();
        assert_eq!(serde_json::to_value(&user).unwrap()["content"], "hello");
        let result: Message = serde_json::from_str(
            r#"{"role":"toolResult","toolCallId":"c1","toolName":"read","content":[{"type":"image","data":"AA==","mimeType":"image/png"}],"isError":false,"timestamp":3}"#,
        )
        .unwrap();
        assert!(matches!(result, Message::ToolResult(ToolResultMessage { is_error: false, .. })));
    }

    #[test]
    fn usage_cost_uses_per_million_prices() {
        let mut usage = Usage { input: 1_000_000, output: 500_000, ..Usage::default() };
        usage.apply_cost(&ModelCost { input: 3.0, output: 15.0, cache_read: 0.3, cache_write: 3.75 });
        assert_eq!(usage.cost.input, 3.0);
        assert_eq!(usage.cost.output, 7.5);
        assert_eq!(usage.cost.total, 10.5);
    }
}
