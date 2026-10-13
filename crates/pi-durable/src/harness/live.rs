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
use crate::types::{SubmissionSettlement, TaskOutcome, TaskRecord};

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
}

fn running(slots: Option<&Json>) -> bool {
    let slots = slots.and_then(Json::as_array);
    slots.is_some_and(|slots| slots.iter().any(|slot| slot.get("status").and_then(Json::as_str) == Some("running")))
}

/// A complete base whenever nothing runs: no generation and no running slot (spec section 8.2).
pub static LIVE_DOC: LazyLock<DocToken<LiveState, LatestConversation>> = LazyLock::new(|| {
    let spec = DocSpec::new("pi.live", 1, LatestConversation(LatestFork::Initial), LiveState::default).checkpoint_when(
        |value, _, _| {
            Ok(value.get("generation").is_none() && !running(value.get("tools")) && !running(value.get("nestedTools")))
        },
    );
    define_doc(spec).unwrap_or_else(|_| unreachable!("pi.live is a valid definition"))
});

pub type LiveDraft = Draft<LiveState>;

/// End the run owned by `task_id`: settle each of its inputs and remove `run`. Always removes the presentation.
pub fn end_run(tx: &Tx, live: &mut LiveState, task_id: TaskId, settlement: SubmissionSettlement) -> DurableResult<()> {
    if let Some(run) = live.run.take_if(|run| run.task_id == task_id) {
        for id in run.inputs {
            tx.settle_submission(id, settlement.clone())?;
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

/// Harness cleanup for an outcome the scheduler writes itself (`faulted` or `orphaned`): a run task ends its run, a
/// tool task's slot is marked done without an entry, and a compaction task's status is removed.
pub async fn settle_scheduler_outcome(tx: &Tx, record: &TaskRecord, outcome: &TaskOutcome) -> DurableResult<()> {
    let kind = record.kind.as_str();
    if kind != TOOL_TASK_KIND && kind != COMPACTION_TASK_KIND && kind != RUN_TASK_KIND {
        return Ok(());
    }
    let draft = tx.doc(&LIVE_DOC, record.conversation_id).await?;
    let mut live = draft.get()?;
    match kind {
        TOOL_TASK_KIND => {
            if let Some(mut slot) = tool_slot(&mut live, record.id) {
                slot.finish();
            }
            remove_nested_slots(&mut live, record.id);
        }
        COMPACTION_TASK_KIND => remove_compaction_status(&mut live, record.id),
        _ => {
            if live.run.as_ref().is_none_or(|run| run.task_id != record.id) {
                return Ok(());
            }
            super::generation::convert_partial(tx, &mut live, record.conversation_id).await?;
            let settlement = match outcome {
                TaskOutcome::Faulted { error } => SubmissionSettlement::Unanswered {
                    reason: "faulted".into(),
                    detail: Some(Json::from(error.message.as_str())),
                },
                TaskOutcome::Orphaned { reason } => SubmissionSettlement::Unanswered { reason: reason.clone(), detail: None },
                _ => SubmissionSettlement::Unanswered { reason: outcome.status().into(), detail: None },
            };
            end_run(tx, &mut live, record.id, settlement)?;
        }
    }
    draft.set(&live)
}
