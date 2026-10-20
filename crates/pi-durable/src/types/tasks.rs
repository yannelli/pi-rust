//! Durable task records and their state machine (spec section 5).

use pi_chord::{Json, JsonMap};
use pi_telemetry::TelemetryCarrier;
use serde::{Deserialize, Serialize};

use crate::ids::{ConversationId, TaskId};

/// How a waiting task treats the tasks it waits on (spec section 5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
}

/// The `status` of a task state, used by task queries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Sleeping,
    Completing,
    Terminal,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Sleeping => "sleeping",
            Self::Completing => "completing",
            Self::Terminal => "terminal",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "waiting" => Self::Waiting,
            "sleeping" => Self::Sleeping,
            "completing" => Self::Completing,
            "terminal" => Self::Terminal,
            _ => return None,
        })
    }
}

/// JSON-safe error snapshot persisted instead of a runtime error object.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TaskOutcomeError {
    pub message: String,
    /// Optional structured diagnostic data for inspection or recovery.
    #[serde(default, deserialize_with = "super::present_json::deserialize", skip_serializing_if = "Option::is_none")]
    pub detail: Option<Json>,
}

impl TaskOutcomeError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), detail: None }
    }
}

/// Durable reason and optional result recorded when a task becomes terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskOutcome {
    Completed {
        /// Absent when the task's result is `undefined` upstream.
        #[serde(
            default,
            deserialize_with = "super::present_json::deserialize",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<Json>,
    },
    /// Expected task or domain failure explicitly committed by its implementation.
    Failed {
        error: TaskOutcomeError,
        #[serde(
            default,
            deserialize_with = "super::present_json::deserialize",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<Json>,
    },
    /// Explicit cancellation handled by the task's abort protocol.
    Aborted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(
            default,
            deserialize_with = "super::present_json::deserialize",
            skip_serializing_if = "Option::is_none"
        )]
        result: Option<Json>,
    },
    /// Task that cannot resume because its definition or migration is unavailable.
    Orphaned { reason: String },
    /// Runtime-detected contract failure, such as an uncaught error or no durable progress.
    Faulted { error: TaskOutcomeError },
}

impl TaskOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::Aborted { .. } => "aborted",
            Self::Orphaned { .. } => "orphaned",
            Self::Faulted { .. } => "faulted",
        }
    }

    pub fn completed(result: impl Into<Json>) -> Self {
        Self::Completed { result: Some(result.into()) }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::Failed { error: TaskOutcomeError::new(message), result: None }
    }

    pub fn aborted(reason: Option<String>) -> Self {
        Self::Aborted { reason, result: None }
    }

    pub fn faulted(message: impl Into<String>) -> Self {
        Self::Faulted { error: TaskOutcomeError::new(message) }
    }

    /// The result of any outcome that carries one.
    pub fn result(&self) -> Option<&Json> {
        match self {
            Self::Completed { result } | Self::Failed { result, .. } | Self::Aborted { result, .. } => result.as_ref(),
            Self::Orphaned { .. } | Self::Faulted { .. } => None,
        }
    }
}

/// Complete durable execution state of a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum TaskState {
    /// Eligible for scheduling.
    Pending { checkpoint: Json },
    /// Reserved by one in-memory task invocation.
    Running { checkpoint: Json },
    /// Parked without an invocation until every task in `on` is terminal; then resumes at `checkpoint`.
    Waiting { checkpoint: Json, on: Vec<TaskId>, policy: JoinPolicy },
    /// Parked without an invocation until the Harness clock reaches `until`, in milliseconds on the
    /// `HarnessOptions::now` clock; then resumes at `checkpoint`. An abort mark does not wait for it.
    Sleeping { checkpoint: Json, until: f64 },
    /// Outcome decided; becomes terminal once no ordinary owned work below is live. Runs no more code.
    Completing { outcome: TaskOutcome },
    /// Permanently settled durable result receipt.
    Terminal { outcome: TaskOutcome },
}

impl TaskState {
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::Pending { .. } => TaskStatus::Pending,
            Self::Running { .. } => TaskStatus::Running,
            Self::Waiting { .. } => TaskStatus::Waiting,
            Self::Sleeping { .. } => TaskStatus::Sleeping,
            Self::Completing { .. } => TaskStatus::Completing,
            Self::Terminal { .. } => TaskStatus::Terminal,
        }
    }

    /// The checkpoint of a live state that has one.
    pub fn checkpoint(&self) -> Option<&Json> {
        match self {
            Self::Pending { checkpoint }
            | Self::Running { checkpoint }
            | Self::Waiting { checkpoint, .. }
            | Self::Sleeping { checkpoint, .. } => Some(checkpoint),
            Self::Completing { .. } | Self::Terminal { .. } => None,
        }
    }

    /// The outcome of a completing or terminal state.
    pub fn outcome(&self) -> Option<&TaskOutcome> {
        match self {
            Self::Completing { outcome } | Self::Terminal { outcome } => Some(outcome),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Terminal { .. })
    }
}

/// Why an abort mark was set when no abort request set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AbortReason {
    /// The task was abandoned after a restart (`TaskOptions::abandon_on_restart`) or sits below one.
    Restart,
}

/// Complete replacement record for one durable task state machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskRecord {
    pub id: TaskId,
    pub conversation_id: ConversationId,
    /// Registered task definition name.
    pub kind: String,
    /// Definition version used to migrate live input and checkpoints.
    pub version: u32,
    /// Original task input, kept while the task is live or terminal.
    pub input: Json,
    /// Owning task of a child task; absent for a task its conversation owns. Immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    /// Whether this conversation-owned task is excluded from ordinary idle waits, conversation aborts, and cascades.
    pub background: bool,
    /// Durable abort mark checked before run-mode progress is committed.
    pub abort_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<AbortReason>,
    /// Set from `TaskOptions::abandon_on_restart`; upstream stores only `true`. Immutable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandon_on_restart: Option<bool>,
    pub state: TaskState,
    /// Small first-writer-wins values kept while the task can run; absent once completing or terminal.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "super::optional_map")]
    pub memos: Option<JsonMap>,
    /// Wall-clock milliseconds of the first change to `running`, stamped by the Session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<f64>,
    /// Wall-clock milliseconds of the change to `terminal`, stamped by the Session; absent while live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<f64>,
    /// How many times in a row a crash, eviction, or reset cut the task off while it ran, without the task changing
    /// its state since; open counts them, and a close does not. At `HarnessSettings::restart_limit` the task is
    /// parked as blocked until `Harness::retry_task()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    /// Adapter-owned context that created this task; phase invocations extract it as their parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TelemetryCarrier>,
    /// Immutable creation-time links for the task's first invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<TelemetryCarrier>>,
}

impl TaskRecord {
    pub fn status(&self) -> TaskStatus {
        self.state.status()
    }

    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}
