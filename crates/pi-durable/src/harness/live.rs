//! The `pi.live` document: run control and presentation of the current generation and tool round (upstream
//! `harness/live.ts`).

use std::collections::HashSet;
use std::sync::LazyLock;

use pi_ai::Usage;
use pi_chord::{Json, JsonMap};
use serde::{Deserialize, Serialize};

use super::types::{CompactionReason, ToolDiagnostic};
use crate::documents::{DocSpec, DocToken, LatestConversation, LatestFork, define_doc};
use crate::errors::DurableResult;
use crate::ids::{EntryId, SubmissionId, TaskId};
use crate::session::{Draft, Tx};
use crate::types::{CommitOptions, SubmissionSettlement, TaskOutcome, TaskRecord};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotStatus {
    Pending,
    Running,
    Done,
}

/// Presentation of one tool call of the current round.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSlot {
    pub call_id: String,
    pub name: String,
    /// Absent for a call not started yet and for a call its request did not offer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub status: SlotStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
    /// Result entry once done; absent when the tool task faulted or was orphaned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
}

impl ToolSlot {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>, status: SlotStatus) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            task_id: None,
            status,
            output: None,
            dropped_bytes: None,
            dropped_lines: None,
            details: None,
            diagnostics: None,
            entry: None,
        }
    }
}

/// How a nested call ended: an error flag, its execution time, its spend, and up to 500 characters of error text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSummary {
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Presentation of one nested call. `call_id` is `<parentCallId>/<key>`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NestedToolSlot {
    pub call_id: String,
    pub parent_call_id: String,
    pub parent_task_id: TaskId,
    pub name: String,
    pub task_id: TaskId,
    #[serde(with = "crate::harness::util::json_map")]
    pub arguments: JsonMap,
    pub status: SlotStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_lines: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Vec<ToolDiagnostic>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<NestedToolSummary>,
}

/// Durable backoff before the next request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryState {
    pub at: f64,
    pub error: String,
}

/// Presentation of one live compaction task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStatus {
    pub task_id: TaskId,
    pub reason: CompactionReason,
    /// Whether a generation waits for it.
    pub blocking: bool,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryState>,
}

/// The newest compaction that failed since a compaction last produced a summary (spec section 8.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionFailure {
    pub task_id: TaskId,
    pub reason: CompactionReason,
    /// The failed or faulted outcome's error message, or why the scheduler orphaned the task.
    pub error: String,
}

/// Why a run ended without an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureReason {
    /// The model's response failed, after any retries, or the compaction a request needed failed.
    ModelError,
    /// No model is configured, or the configured one is not available.
    NoModel,
    /// The run was aborted (`Conversation::abort()`, `abort_task()`).
    Aborted,
    /// A queued reset started a new context at the post-tools boundary, before an answer.
    Reset,
    /// The run's task threw where it cannot recover, or storage rejected its commit.
    Faulted,
    /// The run's task was aborted while no registered definition could take it.
    Orphaned,
    /// The host stopped during requests or polls more often than the retry policy allows.
    Interrupted,
    /// The generation was cut off repeatedly without committing progress.
    RestartLimit,
}

impl RunFailureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelError => "model_error",
            Self::NoModel => "no_model",
            Self::Aborted => "aborted",
            Self::Reset => "reset",
            Self::Faulted => "faulted",
            Self::Orphaned => "orphaned",
            Self::Interrupted => "interrupted",
            Self::RestartLimit => "restart_limit",
        }
    }
}

/// The newest run that ended without an answer (spec section 8.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunFailure {
    /// The task that ended the run: the run's task at its end.
    pub task_id: TaskId,
    /// The run's inputs, settled `unanswered`.
    pub inputs: Vec<SubmissionId>,
    pub reason: RunFailureReason,
    /// `model_error`: the error text; `no_model`: which model is missing; `faulted`: the error message; `orphaned`:
    /// why the task could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// How a run ends: with an answer, or without one, settling its inputs `unanswered` and recording why.
#[derive(Clone, Debug, PartialEq)]
pub enum RunEnd {
    Done {
        answer: EntryId,
    },
    Unanswered {
        reason: RunFailureReason,
        detail: Option<String>,
        /// The inputs' settlement; default `unanswered` with `reason` and `detail`.
        settlement: Option<SubmissionSettlement>,
    },
}

impl RunEnd {
    /// An unanswered end whose inputs settle with `reason` and `detail`.
    pub fn unanswered(reason: RunFailureReason, detail: Option<&str>) -> Self {
        Self::Unanswered { reason, detail: detail.map(str::to_owned), settlement: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveRun {
    pub task_id: TaskId,
    pub inputs: Vec<SubmissionId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeferredPoll {
    pub poll_at: f64,
}

/// Presentation of the current generation request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveGeneration {
    pub attempt: u32,
    /// Committed throttled partial of the in-flight response, an `AssistantMessage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Json>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<DeferredPoll>,
}

impl LiveGeneration {
    pub fn new(attempt: u32) -> Self {
        Self { attempt, message: None, retry: None, deferred: None }
    }
}

/// Built-in live conversation state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveState {
    /// The task that settles the run's inputs, and those inputs; present exactly while busy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<LiveRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<LiveGeneration>,
    /// The current tool round in call order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSlot>>,
    /// Nested calls of running tool calls, in creation order, so task IDs ascend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_tools: Option<Vec<NestedToolSlot>>,
    /// Live compaction tasks in task ID order; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compactions: Option<Vec<CompactionStatus>>,
    /// The newest compaction failure, so a client can show it after the task ended. A compaction that produces a
    /// summary removes it; one that is aborted or finds nothing to compact leaves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_failure: Option<CompactionFailure>,
    /// Why the newest run ended without an answer. Set in the commit that ends the run; removed when a later commit
    /// starts a run. A run started in the same commit, by `afterFailedRun: "continue"` or after a reset, keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_failure: Option<RunFailure>,
}

fn running(slots: Option<&Json>) -> bool {
    let slots = slots.and_then(Json::as_array);
    slots.is_some_and(|slots| slots.iter().any(|slot| slot.get("status").and_then(Json::as_str) == Some("running")))
}

/// A complete base whenever nothing runs: no generation and no running slot (spec section 8.2).
/// How the progress throttles commit a partial answer and running tool output into `pi.live`: deferred, so storage gets
/// them together, at the next commit that reads or writes `pi.live` or after `settings.progress.persist_within_ms`.
/// Every commit that consumes or clears streamed fields reads `pi.live`, so it stores them with its own changes.
pub const PROGRESS_COMMIT: CommitOptions = CommitOptions::DEFERRED;

pub static LIVE_DOC: LazyLock<DocToken<LiveState, LatestConversation>> = LazyLock::new(|| {
    let spec =
        DocSpec::new("pi.live", 1, LatestConversation(LatestFork::Initial), LiveState::default, |value, _, _| {
            Ok(value.get("generation").is_none() && !running(value.get("tools")) && !running(value.get("nestedTools")))
        });
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.live is a valid definition"))
});

pub type LiveDraft = Draft<LiveState>;

/// End the run owned by `task_id`: settle each of its inputs, remove `run`, and, without an answer, record
/// `run_failure`. Always removes the presentation, which belongs to the ending run.
pub fn end_run(tx: &Tx, live: &mut LiveState, task_id: TaskId, end: RunEnd) -> DurableResult<()> {
    if let Some(run) = live.run.take_if(|run| run.task_id == task_id) {
        let settlement = match &end {
            RunEnd::Done { answer } => SubmissionSettlement::Done { answer: *answer },
            RunEnd::Unanswered { settlement: Some(settlement), .. } => settlement.clone(),
            RunEnd::Unanswered { reason, detail, settlement: None } => SubmissionSettlement::Unanswered {
                reason: reason.as_str().into(),
                detail: detail.as_deref().map(Json::from),
            },
        };
        for id in &run.inputs {
            tx.settle_submission(*id, settlement.clone())?;
        }
        if let RunEnd::Unanswered { reason, detail, .. } = end {
            live.run_failure = Some(RunFailure { task_id, inputs: run.inputs, reason, detail });
        }
    }
    live.generation = None;
    live.tools = None;
    live.nested_tools = None;
    Ok(())
}

pub fn add_compaction_status(live: &mut LiveState, status: CompactionStatus) {
    live.compactions.get_or_insert_with(Vec::new).push(status);
}

pub fn compaction_status(live: &mut LiveState, task_id: TaskId) -> Option<&mut CompactionStatus> {
    live.compactions.as_mut()?.iter_mut().find(|status| status.task_id == task_id)
}

/// Remove the status of compaction task `task_id`, and the list once empty.
pub fn remove_compaction_status(live: &mut LiveState, task_id: TaskId) {
    let Some(statuses) = &mut live.compactions else { return };
    statuses.retain(|status| status.task_id != task_id);
    if statuses.is_empty() {
        live.compactions = None;
    }
}

/// Remove the status of compaction task `task_id` and record that it failed with `error`.
pub fn fail_compaction_status(live: &mut LiveState, task_id: TaskId, error: impl Into<String>) {
    if let Some(status) = compaction_status(live, task_id) {
        let failure = CompactionFailure { task_id: status.task_id, reason: status.reason, error: error.into() };
        live.compaction_failure = Some(failure);
    }
    remove_compaction_status(live, task_id);
}

/// A tool task's slot, model-issued or nested.
pub enum SlotRef<'a> {
    Model(&'a mut ToolSlot),
    Nested(&'a mut NestedToolSlot),
}

/// The slot of tool task `task_id`, if `pi.live` still lists it. Nested slots ascend by task ID.
pub fn tool_slot(live: &mut LiveState, task_id: TaskId) -> Option<SlotRef<'_>> {
    if let Some(index) = live.tools.as_ref().and_then(|tools| tools.iter().position(|s| s.task_id == Some(task_id))) {
        return live.tools.as_mut().map(|tools| SlotRef::Model(&mut tools[index]));
    }
    let nested = live.nested_tools.as_mut()?;
    let index = nested.binary_search_by(|slot| slot.task_id.cmp(&task_id)).ok()?;
    Some(SlotRef::Nested(&mut nested[index]))
}

macro_rules! progress_fields {
    ($slot:expr, $body:ident) => {
        match $slot {
            SlotRef::Model(slot) => $body!(slot),
            SlotRef::Nested(slot) => $body!(slot),
        }
    };
}

impl SlotRef<'_> {
    pub fn set_status(&mut self, status: SlotStatus) {
        macro_rules! apply {
            ($s:ident) => {
                $s.status = status
            };
        }
        progress_fields!(self, apply);
    }

    /// Remove what a tool published while running.
    pub fn clear_progress(&mut self) {
        macro_rules! apply {
            ($s:ident) => {{
                $s.output = None;
                $s.dropped_bytes = None;
                $s.dropped_lines = None;
                $s.details = None;
                $s.diagnostics = None;
            }};
        }
        progress_fields!(self, apply);
    }

    /// Mark the slot done and clear its running output.
    pub fn finish(&mut self) {
        self.set_status(SlotStatus::Done);
        self.clear_progress();
    }

    /// The durable progress: output, dropped bytes and lines, details, and diagnostics.
    pub fn progress(&self) -> SlotProgress {
        macro_rules! read {
            ($s:ident) => {
                SlotProgress {
                    output: $s.output.clone(),
                    dropped_bytes: $s.dropped_bytes.unwrap_or(0),
                    dropped_lines: $s.dropped_lines.unwrap_or(0),
                    details: $s.details.clone(),
                    diagnostics: $s.diagnostics.clone().unwrap_or_default(),
                }
            };
        }
        match self {
            SlotRef::Model(slot) => read!(slot),
            SlotRef::Nested(slot) => read!(slot),
        }
    }

    /// Write running progress: the output, what the bounds dropped, the details, and new diagnostics.
    pub fn publish(&mut self, output: &str, dropped: (u64, u64), details: Option<&Json>, added: &[ToolDiagnostic]) {
        macro_rules! apply {
            ($s:ident) => {{
                if $s.output.as_deref().unwrap_or("") != output {
                    $s.output = Some(output.to_owned());
                }
                if dropped.0 > 0 {
                    $s.dropped_bytes = Some(dropped.0);
                }
                if dropped.1 > 0 {
                    $s.dropped_lines = Some(dropped.1);
                }
                if let Some(details) = details {
                    $s.details = Some(details.clone());
                }
                if !added.is_empty() {
                    $s.diagnostics.get_or_insert_with(Vec::new).extend(added.iter().cloned());
                }
            }};
        }
        progress_fields!(self, apply);
    }
}

/// Progress a slot published, as interruption and abort results read it.
#[derive(Clone, Debug, Default)]
pub struct SlotProgress {
    pub output: Option<String>,
    pub dropped_bytes: u64,
    pub dropped_lines: u64,
    pub details: Option<Json>,
    pub diagnostics: Vec<ToolDiagnostic>,
}

/// Remove the nested slots below tool task `task_id`, transitively; the list holds parents before children.
pub fn remove_nested_slots(live: &mut LiveState, task_id: TaskId) {
    let Some(nested) = &mut live.nested_tools else { return };
    let mut removed = HashSet::from([task_id]);
    for slot in nested.iter() {
        if removed.contains(&slot.parent_task_id) {
            removed.insert(slot.task_id);
        }
    }
    nested.retain(|slot| !removed.contains(&slot.parent_task_id));
    if nested.is_empty() {
        live.nested_tools = None;
    }
}

const TOOL_TASK_KIND: &str = "pi.tool";
const COMPACTION_TASK_KIND: &str = "pi.compaction";
const RUN_TASK_KIND: &str = "pi.generation";

/// Built-in phases end themselves at the limit; the scheduler faults other task kinds.
pub(crate) fn limits_restarts(kind: &str) -> bool {
    matches!(kind, TOOL_TASK_KIND | COMPACTION_TASK_KIND | RUN_TASK_KIND)
}

pub(crate) fn cut_off_too_often(task: &TaskRecord, runtime: &super::scheduler::TaskRuntime) -> bool {
    task.restarts.is_some_and(|count| f64::from(count) >= runtime.settings().restart_limit)
}

/// Cleanup is attempted at the limit; beyond it, that cleanup may be what keeps failing.
pub(crate) fn salvageable(task: &TaskRecord, runtime: &super::scheduler::TaskRuntime) -> bool {
    task.restarts.is_none_or(|count| f64::from(count) <= runtime.settings().restart_limit)
}

pub(crate) fn cut_off_count(task: &TaskRecord) -> String {
    match task.restarts.unwrap_or(0) {
        1 => "once".into(),
        count => format!("{count} times in a row"),
    }
}

/// Harness cleanup for an outcome the scheduler writes itself (`faulted` or `orphaned`): a run task ends its run, a
/// tool task's slot is marked done without an entry, and a compaction task's status is removed and its failure
/// recorded.
pub async fn settle_scheduler_outcome(tx: &Tx, record: &TaskRecord, outcome: &TaskOutcome) -> DurableResult<()> {
    let kind = record.kind.as_str();
    if kind != TOOL_TASK_KIND && kind != COMPACTION_TASK_KIND && kind != RUN_TASK_KIND {
        return Ok(());
    }
    let draft = tx.doc(&LIVE_DOC, record.conversation_id).await?;
    let mut live = draft.get()?;
    let cut_off = matches!(outcome, TaskOutcome::Faulted { error }
        if crate::types::InterruptionReason::from_error(error) == Some(crate::types::InterruptionReason::RestartLimit));
    match kind {
        TOOL_TASK_KIND => {
            if let Some(mut slot) = tool_slot(&mut live, record.id) {
                slot.finish();
            }
            remove_nested_slots(&mut live, record.id);
        }
        COMPACTION_TASK_KIND => {
            let error = match outcome {
                TaskOutcome::Faulted { error } => error.message.clone(),
                TaskOutcome::Orphaned { reason } => reason.clone(),
                other => other.status().to_owned(),
            };
            if cut_off {
                remove_compaction_status(&mut live, record.id);
            } else {
                fail_compaction_status(&mut live, record.id, error);
            }
        }
        _ => {
            if live.run.as_ref().is_none_or(|run| run.task_id != record.id) {
                return Ok(());
            }
            if !cut_off {
                super::generation::convert_partial(tx, &mut live, record.conversation_id).await?;
            }
            let end = match outcome {
                TaskOutcome::Faulted { error } => RunEnd::unanswered(
                    if cut_off { RunFailureReason::Aborted } else { RunFailureReason::Faulted },
                    Some(&error.message),
                ),
                TaskOutcome::Orphaned { reason } => RunEnd::Unanswered {
                    reason: RunFailureReason::Orphaned,
                    detail: Some(reason.clone()),
                    settlement: Some(SubmissionSettlement::Unanswered { reason: reason.clone(), detail: None }),
                },
                _ => RunEnd::Unanswered {
                    reason: RunFailureReason::Orphaned,
                    detail: Some(outcome.status().into()),
                    settlement: Some(SubmissionSettlement::Unanswered {
                        reason: outcome.status().into(),
                        detail: None,
                    }),
                },
            };
            end_run(tx, &mut live, record.id, end)?;
        }
    }
    draft.set(&live)
}
