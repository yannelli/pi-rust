//! Persisted and published data shapes. JSON field names match upstream `src/types.ts`.

use pi_agent::{AgentEvent, AgentToolResult};
use pi_ai::{AssistantMessage, ContentBlock, JsonObject, Message, Model, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    AwaitingApproval,
    Paused,
    Interrupted,
    Disposed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolEffect {
    #[default]
    Read,
    Mutation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationPhase {
    Pending,
    Approval,
    Executing,
    Uncertain,
    Completed,
}

/// A JSON-compatible tool outcome (upstream `AgentToolResult<JsonValue>`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    #[serde(default)]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl ToolResult {
    pub fn text(text: impl Into<String>, details: Value) -> Self {
        Self { content: vec![ContentBlock::text(text)], details, is_error: None }
    }

    /// A value reported as its JSON text, with the value itself as details.
    pub fn json(value: Value) -> Self {
        Self::text(value.to_string(), value)
    }

    /// A known failed outcome.
    pub fn failure(message: impl Into<String>) -> Self {
        Self { content: vec![ContentBlock::text(message)], details: Value::Null, is_error: Some(true) }
    }

    pub fn is_error(&self) -> bool {
        self.is_error == Some(true)
    }

    pub(crate) fn into_agent(self, terminate: bool) -> AgentToolResult {
        AgentToolResult {
            content: self.content,
            details: Some(self.details),
            is_error: self.is_error == Some(true),
            terminate,
            ..AgentToolResult::default()
        }
    }

    pub(crate) fn from_agent(result: AgentToolResult) -> Self {
        Self {
            content: result.content,
            details: result.details.unwrap_or(Value::Null),
            is_error: result.is_error.then_some(true),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInvocation {
    pub session_id: String,
    pub id: String,
    pub name: String,
    pub args: JsonObject,
    pub effect: ToolEffect,
    pub phase: InvocationPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
}

/// A host or tool decision about an uncertain invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Reconciliation {
    /// The effect happened; the saved result is reused.
    Completed { result: ToolResult },
    /// The effect did not happen; execution becomes pending and approval is requested again.
    NotApplied,
    /// Evidence is insufficient; resume stays blocked.
    Unknown,
}

/// A persisted session checkpoint (`version: 1`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub version: u32,
    pub id: String,
    pub revision: u64,
    pub sequence: u64,
    pub status: SessionStatus,
    pub model: Model,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<AssistantMessage>,
    /// Saved user messages for the next turn after the current tool batch.
    pub steering: Vec<Message>,
    /// Saved user messages to run after current work and steering finish.
    pub follow_ups: Vec<Message>,
    pub invocations: Vec<ToolInvocation>,
    pub usage: Vec<Usage>,
    pub context_start: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub needs_response: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub status: SessionStatus,
    pub revision: u64,
}

/// Event payloads; [`SessionEvent`] adds the version, session ID, and sequence.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum EventPayload {
    Agent {
        event: AgentEvent,
    },
    Session {
        status: SessionStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Approval {
        invocation: ToolInvocation,
    },
    ApprovalDecision {
        invocation_id: String,
        approved: bool,
    },
    ToolProgress {
        invocation_id: String,
        result: ToolResult,
    },
    Recovery {
        invocation: ToolInvocation,
    },
    Usage {
        usage: Usage,
    },
    Queue {
        steering: Vec<Message>,
        follow_ups: Vec<Message>,
    },
    Compaction {
        context_start: usize,
        summary: String,
    },
}

/// A live, versioned notification. Checkpoints precede delivery; on a sequence gap,
/// redraw from `snapshot()`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    #[serde(flatten)]
    pub payload: EventPayload,
    pub version: u32,
    pub session_id: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceDocument {
    pub path: String,
    pub text: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: u64,
    pub text: String,
}

/// One provider HTTP request issued through the host's [`crate::NetworkAdapter`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NetworkRequest {
    pub url: String,
    /// Always `POST`.
    pub method: String,
    pub headers: std::collections::BTreeMap<String, String>,
    pub body: String,
}

/// Output of a [`crate::Compactor`]: the summary and the usage that produced it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compaction {
    pub summary: String,
    pub usage: Option<Usage>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleState {
    Active,
    Background,
    Suspend,
    Terminate,
}
