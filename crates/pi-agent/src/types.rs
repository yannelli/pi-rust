//! Tool, result, and event types shared by the agent loop and its hosts.

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::{AssistantMessage, AssistantMessageEvent, ContentBlock, Message, Tool, ToolCall, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// Result of one tool execution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Marks the result as an error without failing the tool call.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// When every result in a batch sets this, the loop ends after the batch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub terminate: bool,
}

impl AgentToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(text)], ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: true, ..Self::text(text) }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Receives partial results while a tool runs.
pub type ToolUpdateCallback = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

/// A tool the agent can call. Returning `Err` reports the error text to the model.
#[async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;

    /// Human-readable name for display.
    fn label(&self) -> &str {
        self.name()
    }

    fn description(&self) -> &str;

    /// JSON Schema for the arguments. Arguments are validated before `execute`.
    fn parameters(&self) -> Value;

    /// `Sequential` forces the whole batch containing this tool to run in order.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult>;

    fn declaration(&self) -> Tool {
        Tool { name: self.name().into(), description: self.description().into(), parameters: self.parameters() }
    }
}

pub type SharedTool = Arc<dyn AgentTool>;

/// Events emitted by the loop, in order. Field names match the TypeScript pi.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Vec<Message> },
    TurnStart,
    TurnEnd { message: Message, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: Message },
    MessageUpdate { message: Message, assistant_message_event: AssistantMessageEvent },
    MessageEnd { message: Message },
    ToolExecutionStart { tool_call_id: String, tool_name: String, args: Value },
    ToolExecutionUpdate { tool_call_id: String, tool_name: String, args: Value, partial_result: AgentToolResult },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
}

/// Input to `before_tool_call`.
pub struct BeforeToolCall<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
}

/// `block` skips execution and reports `reason` as an error result.
#[derive(Clone, Debug, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
}

/// Input to `after_tool_call`.
pub struct AfterToolCall<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
}

/// Fields set here replace the executed result's fields.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}
