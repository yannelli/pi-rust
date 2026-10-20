//! Data types of the Harness (upstream `harness/types.ts`): agent state and changes, tool results, and inspection
//! records. Settings live in [`super::settings`]; tools, sections, extensions, and tasks in their own modules.

use std::sync::Arc;

use pi_ai::{ContentBlock, Message, ThinkingLevel, Usage, UserContent};
use pi_chord::Json;
use serde::{Deserialize, Serialize};

use super::define::{Extension, PromptSection};
use super::tool_registration::ToolRegistration;
use crate::ids::{EntryId, SubmissionId, TaskId};
use crate::types::{EntryDraft, EntryRecord, SubmissionRecord, TaskRecord};

/// Provider and model ID resolved through `Models`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model_id: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self { provider: provider.into(), model_id: model_id.into() }
    }
}

/// User message content: text or content blocks.
pub type UserInput = UserContent;

/// How input submitted to a busy conversation is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WhenBusy {
    Steer,
    FollowUp,
    Reject,
}

/// Host submission: user input that may start a run, or a passive entry write.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionDraft {
    Input { request_id: Option<String>, content: UserInput, when_busy: Option<WhenBusy> },
    Write { request_id: Option<String>, entry: EntryDraft },
}

impl SubmissionDraft {
    /// User text with the default busy behavior (`followUp`).
    pub fn input(content: impl Into<String>) -> Self {
        Self::Input { request_id: None, content: UserContent::Text(content.into()), when_busy: None }
    }

    pub fn write(entry: EntryDraft) -> Self {
        Self::Write { request_id: None, entry }
    }

    pub fn request_id(&self) -> Option<&str> {
        match self {
            Self::Input { request_id, .. } | Self::Write { request_id, .. } => request_id.as_deref(),
        }
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        match &mut self {
            Self::Input { request_id, .. } | Self::Write { request_id, .. } => *request_id = Some(id.into()),
        }
        self
    }

    pub fn with_when_busy(mut self, mode: WhenBusy) -> Self {
        if let Self::Input { when_busy, .. } = &mut self {
            *when_busy = Some(mode);
        }
        self
    }
}

/// Post-tools controls requested by a tool result.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolControl {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add_tools: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticSeverity {
    Info,
    Warn,
    Error,
}

impl DiagnosticSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

/// Remark about a call for the model and the UI, such as truncation or a spill path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolDiagnostic {
    pub severity: DiagnosticSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

impl ToolDiagnostic {
    pub fn new(severity: DiagnosticSeverity, message: impl Into<String>) -> Self {
        Self { severity, code: None, message: message.into() }
    }

    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
}

/// What a tool returns: `output` for the model, `structured_output` for programs, and `details` for UIs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolExecutionResult {
    /// For the model. `None`: the kept `output()` text.
    pub output: Option<Vec<ContentBlock>>,
    pub structured_output: Option<Json>,
    pub is_error: Option<bool>,
    /// `None`: the last `details()` value.
    pub details: Option<Json>,
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
    pub usage: Option<Usage>,
    pub control: Option<ToolControl>,
}

impl ToolExecutionResult {
    /// A result with one text item.
    pub fn text(text: impl Into<String>) -> Self {
        Self { output: Some(vec![ContentBlock::text(text)]), ..Self::default() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { is_error: Some(true), ..Self::text(text) }
    }
}

/// Result of a nested call, as `execute_tool()` returns it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolExecutionResult {
    pub task_id: TaskId,
    /// A stored `null` stays `Some(Json::Null)`; absent means no structured output.
    #[serde(
        default,
        deserialize_with = "crate::types::present_json::deserialize",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured_output: Option<Json>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
    pub diagnostics: Vec<ToolDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Whether the tools of one round run at once or one after another in call order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

/// How many queued items of one mode a boundary places.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    #[serde(rename = "all")]
    All,
    #[default]
    #[serde(rename = "one-at-a-time")]
    OneAtATime,
}

/// Stored extension selection: exactly these names, or an edit of the host default.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ExtensionSelection {
    List(Vec<String>),
    Edit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        add: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remove: Option<Vec<String>>,
    },
}

/// Stored tool filter: exactly these names, in order, or the names to drop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolFilter {
    List(Vec<String>),
    Remove { remove: Vec<String> },
}

/// Stored choices of one conversation (`pi.agent`); names, not objects. Unset fields follow the host.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<ExtensionSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tools: Option<ToolFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// One field of an [`AgentChange`]: `Set` replaces the stored value and `Clear` removes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Change<T> {
    #[default]
    Keep,
    Clear,
    Set(T),
}

/// A change to `pi.agent`. Upstream passes extension and tool objects; their names are what it stores.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentChange {
    pub model: Change<ModelRef>,
    pub thinking_level: Change<ThinkingLevel>,
    pub extensions: Change<ExtensionSelection>,
    pub tools: Change<ToolFilter>,
    pub model_tools: Change<ToolFilter>,
    pub instructions: Change<String>,
    pub cwd: Change<String>,
}

impl AgentChange {
    pub fn model(model: ModelRef) -> Self {
        Self { model: Change::Set(model), ..Self::default() }
    }
}

/// A conversation's agent resolved against a registry snapshot and the settings.
#[derive(Clone, Default)]
pub struct Agent {
    pub model: Option<ModelRef>,
    pub thinking_level: ThinkingLevel,
    pub extensions: Vec<Arc<Extension>>,
    /// The tools a request offers, in order.
    pub tools: Vec<Arc<ToolRegistration>>,
    /// The tools nested calls resolve among, in order.
    pub callable: Vec<Arc<ToolRegistration>>,
    /// Extension sections, then `instructions` when set.
    pub sections: Vec<Arc<PromptSection>>,
    pub instructions: Option<String>,
    pub cwd: Option<String>,
}

impl Agent {
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|tool| tool.name().to_owned()).collect()
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = |tools: &[Arc<ToolRegistration>]| tools.iter().map(|t| t.name().to_owned()).collect::<Vec<_>>();
        f.debug_struct("Agent")
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("extensions", &self.extensions.iter().map(|e| e.name.clone()).collect::<Vec<_>>())
            .field("tools", &names(&self.tools))
            .field("callable", &names(&self.callable))
            .field("sections", &self.sections.iter().map(|s| s.key.clone()).collect::<Vec<_>>())
            .field("instructions", &self.instructions)
            .field("cwd", &self.cwd)
            .finish()
    }
}

/// Why a compaction runs: `compact()`, a threshold in generation preparation, or a context overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

impl CompactionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Threshold => "threshold",
            Self::Overflow => "overflow",
        }
    }
}

/// The summary entry of a blocking compaction, or the write submission of a conversation-owned one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<SubmissionId>,
}

/// Raw active transcript and derived model context.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextView {
    /// Newest applicable head marker, if any.
    pub head: Option<EntryRecord>,
    /// The head marker followed by non-head entries from its head through the tail.
    pub entries: Vec<EntryRecord>,
    /// Per entry, its model messages after edits and excluded stop reasons.
    pub contributions: Vec<Vec<Message>>,
    /// Model context for the next provider request.
    pub messages: Vec<Message>,
}

/// Why a pending task cannot be reserved under a registry snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    MissingTask,
    TaskTooOld,
    MigrationFailed,
}

impl BlockedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingTask => "missing_task",
            Self::TaskTooOld => "task_too_old",
            Self::MigrationFailed => "migration_failed",
        }
    }
}

/// When a Session next needs a running process (`Harness::wake()`). `active`: some task can run now or is running, so
/// a process that dies must be woken again soon. `at`: the earliest sleep not yet due, when there is one. Neither:
/// nothing needs a process until new work is submitted. Tasks blocked on a missing or unmigratable definition,
/// `completing` tasks, and waits on live tasks do not count; the work they wait for does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Wake {
    pub active: bool,
    pub at: Option<f64>,
}

/// What the scheduler would do with a live task under the current registry.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskInspectionState {
    Running,
    Ready {
        migrates: bool,
    },
    Waiting {
        on: Vec<TaskId>,
    },
    /// Sleeps until the Harness clock reaches `until`.
    Sleeping {
        until: f64,
    },
    Completing,
    Blocked {
        reason: BlockedReason,
        error: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskInspection {
    pub record: TaskRecord,
    pub state: TaskInspectionState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheduling {
    Paused,
    Running,
    Closing,
}

/// Point-in-time view of live work: unfinished tasks and submissions.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessInspection {
    pub scheduling: Scheduling,
    pub tasks: Vec<TaskInspection>,
    /// Queued and placed submissions, in ID order.
    pub submissions: Vec<SubmissionRecord>,
}

/// How a submission withdrawal ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortSubmissionResult {
    Aborted,
    AlreadyPlaced,
    Settled,
    NotFound,
}

/// Whether `abort_task` marked the task or found it terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortTaskResult {
    Marked,
    Terminal,
}
