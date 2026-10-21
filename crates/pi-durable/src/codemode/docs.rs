//! Documents of durable code mode: the run's start data and outcome, the journal of delivered results, and the
//! values scripts keep with `store()`.

use std::sync::LazyLock;

use pi_ai::ContentBlock;
use pi_chord::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::documents::{DocSpec, DocToken, RewindableConversation, TaskScope, define_doc};
use crate::harness::ToolExecutionResult;
use crate::types::DocumentFork;

/// One tool as the script sees it: `tools.<js_name>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclaredTool {
    pub name: String,
    pub js_name: String,
    pub description: String,
}

/// One global as the script sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeclaredGlobal {
    pub name: String,
    pub spread: bool,
}

/// What a call starts with; a rerun starts the VM from exactly this.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeStart {
    pub version: u32,
    pub started_at: f64,
    pub seed: u32,
    /// The VM's fingerprint, recorded by the first run before any call is admitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// `started_at + timeout_ms` when the script set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<f64>,
    pub tools: Vec<DeclaredTool>,
    pub globals: Vec<DeclaredGlobal>,
    pub store: Map<String, Value>,
}

/// A tool result as the run document stores it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredResult {
    pub output: Vec<ContentBlock>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
}

impl StoredResult {
    pub fn error(message: impl Into<String>) -> Self {
        Self { output: vec![ContentBlock::text(message)], is_error: true, details: None }
    }

    pub fn into_tool_result(self) -> ToolExecutionResult {
        ToolExecutionResult {
            output: Some(self.output),
            is_error: Some(self.is_error),
            details: self.details,
            ..ToolExecutionResult::default()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<CodemodeStart>,
    /// The outcome, decided once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<StoredResult>,
}

/// `pi.codemode.run`: the start data, written once before the VM starts, and the outcome, written once.
pub static CODEMODE_RUN_DOC: LazyLock<DocToken<RunState, TaskScope>> = LazyLock::new(|| {
    // The document retires with its task.
    let spec = DocSpec::new("pi.codemode.run", 1, TaskScope, RunState::default, |_, _, _| Ok(false));
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.codemode.run is a valid definition"))
});

/// A global's call and outcome; a tool's result is its nested call's.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalGlobal {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Value>,
    pub outcome: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    /// The VM's call ID. Its result is the entry's position + 1 (`seq`).
    pub id: u64,
    /// The VM clock while it processes this result.
    pub at: f64,
    /// Calls the VM had issued after processing this result; absent until acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issued_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<JournalGlobal>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalState {
    pub entries: Vec<JournalEntry>,
    /// Calls the VM had issued after the script's first run, before any result; absent until acknowledged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_issued: Option<u64>,
}

/// `pi.codemode.journal`: one entry per result delivered to the VM, in delivery order.
pub static CODEMODE_JOURNAL_DOC: LazyLock<DocToken<JournalState, TaskScope>> = LazyLock::new(|| {
    // The document retires with its task.
    let spec = DocSpec::new("pi.codemode.journal", 1, TaskScope, JournalState::default, |_, _, _| Ok(false));
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.codemode.journal is a valid definition"))
});

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StoreState {
    pub values: Map<String, Value>,
}

/// `pi.codemode.store`: what scripts keep with `store()`, per conversation. Rewindable and copied as of the fork
/// point, so a fork sees the values as they were when its entry was committed.
pub static CODEMODE_STORE_DOC: LazyLock<DocToken<StoreState, RewindableConversation>> = LazyLock::new(|| {
    let scope = RewindableConversation(DocumentFork::AsOf);
    let spec =
        DocSpec::new("pi.codemode.store", 1, scope, StoreState::default, |_, _, info| Ok(info.deltas_since_base >= 64));
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.codemode.store is a valid definition"))
});
