//! Typed command results for [`super::RpcClient`], deserialized from upstream JSON shapes.

use pi_ai::{Model, ThinkingLevel, Usage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `clear_queue` data: the texts removed from both queues.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcClearedQueue {
    pub steering: Vec<String>,
    pub follow_up: Vec<String>,
}

/// `cycle_model` data.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcModelCycle {
    pub model: Model,
    pub thinking_level: ThinkingLevel,
    pub is_scoped: bool,
}

/// `compact` data (upstream `CompactionResult`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcCompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    #[serde(default)]
    pub estimated_tokens_after: Option<u64>,
    #[serde(default)]
    pub usage: Option<Usage>,
    #[serde(default)]
    pub details: Option<Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcTokenTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total: u64,
}

/// Context window use; `tokens` and `percent` are unknown right after compaction.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// `get_session_stats` data (upstream `SessionStats`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcSessionStats {
    #[serde(default)]
    pub session_file: Option<String>,
    pub session_id: String,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub total_messages: u64,
    pub tokens: RpcTokenTotals,
    pub cost: f64,
    #[serde(default)]
    pub context_usage: Option<RpcContextUsage>,
}

/// `fork` data: the forked-from prompt text, absent when cancelled.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcForkResult {
    #[serde(default)]
    pub text: Option<String>,
    pub cancelled: bool,
}

/// A user message `fork` can start from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcForkMessage {
    pub entry_id: String,
    pub text: String,
}

/// `get_entries` data: raw session entries in append order and the current leaf.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcEntries {
    pub entries: Vec<Value>,
    pub leaf_id: Option<String>,
}

/// An answer to an `extension_ui_request` dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RpcUiResponse {
    /// The chosen or entered text (select, input, editor).
    Value(String),
    /// The confirm answer.
    Confirmed(bool),
    /// Dismissed; the extension gets its default value.
    Cancelled,
}

impl RpcUiResponse {
    /// The `extension_ui_response` record for request `id`.
    pub fn to_record(&self, id: &str) -> Value {
        match self {
            Self::Value(value) => json!({"type": "extension_ui_response", "id": id, "value": value}),
            Self::Confirmed(confirmed) => json!({"type": "extension_ui_response", "id": id, "confirmed": confirmed}),
            Self::Cancelled => json!({"type": "extension_ui_response", "id": id, "cancelled": true}),
        }
    }
}
