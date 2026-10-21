//! Tool, result, and event types shared by the agent loop and its hosts.

use std::sync::Arc;

use async_trait::async_trait;
use pi_ai::{AssistantMessage, AssistantMessageEvent, ContentBlock, Message, Tool, ToolCall, ToolResultMessage, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::AgentContext;
use crate::nested::ToolContext;
use crate::tool_model::{
    ToolAnnotations, ToolExposure, ToolLoadout, ToolLoadoutChanges, ToolNamespace, ToolReach, ToolReplay,
};

/// Result of one tool execution.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Machine-readable result matching the tool's `output_schema`. Never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
    /// Usage of the tool execution itself, outside the main context accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
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

    pub fn with_structured_content(mut self, structured_content: Value) -> Self {
        self.structured_content = Some(structured_content);
        self
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}

/// Final outcome of a tool call after hooks ran.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentToolCallOutcome {
    pub tool_call: ToolCall,
    pub result: AgentToolResult,
    pub is_error: bool,
    /// Milliseconds `execute` took; `None` when the tool did not run.
    pub duration_ms: Option<u64>,
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
/// Everything but the name, description, parameters, and `execute` has a default, so
/// built-in, extension, MCP, and codemode tools all register the same way.
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

    /// JSON Schema of `structured_content` in successful results. Tools that declare it
    /// should always set `structured_content`; codemode scripts then receive it.
    fn output_schema(&self) -> Option<Value> {
        None
    }

    /// Provider-side constrained sampling request (`constrainedSampling`).
    fn constrained_sampling(&self) -> Option<Value> {
        None
    }

    /// How the model reaches the tool.
    fn exposure(&self) -> ToolExposure {
        ToolExposure::Direct
    }

    /// Group the tool belongs to, for example its MCP server.
    fn namespace(&self) -> Option<&ToolNamespace> {
        None
    }

    /// Hints about what the tool does, for example from an MCP server.
    fn annotations(&self) -> Option<&ToolAnnotations> {
        None
    }

    /// For tools that reach other tools: which ones a call with `input` may reach. `None` for tools
    /// that do not reach other tools.
    fn reaches_tools(&self, _input: &Value) -> Option<ToolReach> {
        None
    }

    /// Whether the tool has `reaches_tools` at all, which listings report without running it.
    fn declares_reaches_tools(&self) -> bool {
        self.reaches_tools(&Value::Object(Default::default())).is_some()
    }

    /// One line for the system prompt's tool list. Tools without one are not listed there.
    fn prompt_snippet(&self) -> Option<&str> {
        None
    }

    /// Guideline bullets added to the system prompt while the tool is active.
    fn prompt_guidelines(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether registering a `Direct` or `ModelOnly` tool activates it.
    fn default_active(&self) -> bool {
        true
    }

    /// Recovery policy for an effect whose outcome is unknown.
    fn replay(&self) -> Option<ToolReplay> {
        None
    }

    /// `Sequential` forces the whole batch containing this tool to run in order.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Compatibility shim for raw arguments, run before schema validation. An `Err` becomes
    /// the tool result error.
    fn prepare_arguments(&self, args: Value) -> anyhow::Result<Value> {
        Ok(args)
    }

    /// Adjust how the loadout is presented while this tool is active, for example to list
    /// the callable tools in its own description. Called whenever the active tools change.
    fn prepare_loadout(&self, _loadout: &ToolLoadout) -> Option<ToolLoadoutChanges> {
        None
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult>;

    /// Execute with access to the call's context, which can run other tools
    /// ([`ToolContext::execute_tool`]). The loop calls this; the default runs `execute`.
    async fn execute_with_context(
        &self,
        context: &ToolContext,
        args: Value,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> anyhow::Result<AgentToolResult> {
        self.execute(context.tool_call_id(), args, cancel, on_update).await
    }

    fn declaration(&self) -> Tool {
        Tool {
            name: self.name().into(),
            description: self.description().into(),
            parameters: self.parameters(),
            constrained_sampling: self.constrained_sampling(),
        }
    }
}

pub type SharedTool = Arc<dyn AgentTool>;

/// Events emitted by the loop, in order. Field names match the TypeScript pi.
/// Events are short-lived, so variants hold messages inline instead of boxing them.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd {
        messages: Vec<Message>,
    },
    TurnStart,
    TurnEnd {
        message: Message,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageStart {
        message: Message,
    },
    MessageUpdate {
        message: Message,
        assistant_message_event: AssistantMessageEvent,
    },
    MessageEnd {
        message: Message,
    },
    /// `parent_tool_call_id` is set for calls a tool made through [`ToolContext::execute_tool`].
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        args: Value,
        partial_result: AgentToolResult,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_call_id: Option<String>,
    },
}

/// Input to `before_tool_call`.
pub struct BeforeToolCall<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    /// Validated arguments for the tool schema.
    pub args: &'a Value,
    /// Agent context at the time the call is prepared.
    pub context: &'a AgentContext,
    /// Set for calls a tool made through [`ToolContext::execute_tool`].
    pub parent_tool_call_id: Option<&'a str>,
}

/// `block` skips execution and reports `reason` as an error result.
#[derive(Clone, Debug, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
    pub terminate: bool,
    /// Arguments to execute instead, without revalidation (upstream hooks mutate `args`).
    pub args: Option<Value>,
}

/// Input to `after_tool_call`.
pub struct AfterToolCall<'a> {
    pub assistant_message: &'a AssistantMessage,
    pub tool_call: &'a ToolCall,
    pub args: &'a Value,
    pub result: &'a AgentToolResult,
    pub is_error: bool,
    /// Agent context at the time the call is finalized.
    pub context: &'a AgentContext,
    /// Set for calls a tool made through [`ToolContext::execute_tool`].
    pub parent_tool_call_id: Option<&'a str>,
}

/// Fields set here replace the executed result's fields. Replacing `content` without
/// `structured_content` drops the structured content, since it may no longer match.
#[derive(Clone, Debug, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub structured_content: Option<Value>,
    pub is_error: Option<bool>,
    pub usage: Option<Usage>,
    pub terminate: Option<bool>,
}
