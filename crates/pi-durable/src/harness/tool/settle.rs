//! Settlement of a tool call, nested call admission, and throttled progress commits into the call's slot.

use std::sync::{Arc, Mutex};

use futures::future::try_join_all;
use pi_ai::{JsonObject, ToolCall};
use pi_chord::{Context, Json};
use pi_telemetry::SpanAttributes;

use super::api::CallState;
use super::decode_input;
use super::result::{append_tool_result, nested_result, summary_of};
use super::{MADE_NESTED_CALLS, NESTED_CALL_DOC, ToolHookCall, ToolTaskInput, ToolTaskResult, json_equal, tool_task};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::harness::live::{
    LIVE_DOC, NestedToolSlot, PROGRESS_COMMIT, SlotProgress, SlotRef, SlotStatus, remove_nested_slots, tool_slot,
};
use crate::harness::progress::{Progress, ProgressClock};
use crate::harness::scheduler::TaskRuntime;
use crate::harness::types::{ToolDiagnostic, ToolExecutionResult};
use crate::harness::usage::{UsageBucket, record_usage};
use crate::harness::util::lock;
use crate::ids::TaskId;
use crate::session::Tx;
use crate::tasks::{TaskOptions, TaskOwnership};
use crate::types::{TaskOutcome, TaskOutcomeError, TaskRecord, TaskState};

/// How a tool task ends. `Failed` (execution failed or was interrupted) records cancellation intent.
#[derive(Clone, Debug)]
pub(crate) enum Ending {
    Completed,
    Aborted,
    Failed { message: String, reason: Option<String> },
}

/// Commit the tool's terminal state and mark its slot done. Nested calls this call left running are aborted
/// first. `build` receives the slot's progress so interruption and abort can report it.
pub(crate) async fn settle(
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    ending: Ending,
    build: impl FnOnce(Option<SlotProgress>) -> ToolExecutionResult + Send + 'static,
    context: &Context,
    duration_ms: Option<u64>,
) -> DurableResult<()> {
    let task_id = runtime.task_id();
    // Every live nested call the task owns, also those an earlier attempt made and this one never called again.
    let name = tool_task().name.clone();
    let nested_ids: Vec<TaskId> = runtime
        .owned_tasks(context)
        .await?
        .into_iter()
        .filter(|record| record.kind == name && matches!(decode_input(record), Ok(ToolTaskInput::Nested { .. })))
        .map(|record| record.id)
        .collect();
    // Read before the settling commit: no admission starts after the end, and those underway were awaited.
    let made_nested = runtime.memo(MADE_NESTED_CALLS).await? == Some(Json::from(true));
    // Mark them all before waiting for any: one may run until a sibling is cancelled.
    try_join_all(nested_ids.iter().map(|id| runtime.abort_owned(*id, context))).await?;
    let (input, call, conversation_id, now) =
        (input.clone(), call.call.clone(), runtime.conversation_id(), runtime.now());
    let change = move |tx: Tx, _| async move {
        let draft = tx.doc(&LIVE_DOC, conversation_id).await?;
        let mut live = draft.get()?;
        let result = build(tool_slot(&mut live, task_id).map(|slot| slot.progress()));
        let settled = match &input {
            ToolTaskInput::Nested { parent, key, .. } => {
                let nested = nested_result(task_id, &result, duration_ms);
                if let Some(usage) = &nested.usage {
                    record_usage(&tx, conversation_id, UsageBucket::Tools, &call.name, usage).await?;
                }
                // The caller is never terminal before its owned nested calls, so its document is open; another task in
                // it is a bug, not a race.
                let state = tx.family_doc(&NESTED_CALL_DOC, *parent, key, &()).await?;
                if state.get()?.task_id != Some(task_id) {
                    return Err(DurableError::error(format!("Nested call {} is not in its caller's index", call.id)));
                }
                state.edit(|state| state.result = Some(nested.clone()))?;
                if let Some(SlotRef::Nested(slot)) = tool_slot(&mut live, task_id) {
                    SlotRef::Nested(&mut *slot).finish();
                    slot.summary = Some(summary_of(&nested, result.output.as_deref().unwrap_or_default()));
                }
                ToolTaskResult::Nested
            }
            ToolTaskInput::Model { .. } => {
                let entry = append_tool_result(&tx, conversation_id, &call, &result, now, duration_ms).await?;
                if let Some(SlotRef::Model(slot)) = tool_slot(&mut live, task_id) {
                    SlotRef::Model(&mut *slot).finish();
                    slot.entry = Some(entry.id);
                }
                let control = match ending {
                    Ending::Completed => result.control.clone(),
                    _ => None,
                };
                ToolTaskResult::Model { entry_id: entry.id, control }
            }
        };
        // A call that made no nested calls, in any attempt, has no slots below it; skipping the scan keeps many leaves
        // linear.
        if made_nested {
            remove_nested_slots(&mut live, task_id);
        }
        draft.set(&live)?;
        let result = Some(encode(&settled)?);
        let outcome = match ending {
            Ending::Aborted => TaskOutcome::Aborted { reason: None, result },
            Ending::Failed { message, reason } => {
                let detail = reason.map(|reason| Json::from(serde_json::json!({ "reason": reason })));
                TaskOutcome::Failed { error: TaskOutcomeError { message, detail }, result }
            }
            Ending::Completed => TaskOutcome::Completed { result },
        };
        Ok(Some(TaskState::Terminal { outcome }))
    };
    runtime.commit(change, context).await
}

/// Create nested call `key` of the call `parent_call_id` in one commit: its tool task, its slot, and its
/// `NESTED_CALL_DOC`. A key already made returns that call's task when it names the same tool and arguments.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_nested_call(
    runtime: &TaskRuntime,
    parent_call_id: &str,
    name: &str,
    args: JsonObject,
    key: &str,
    progress: Option<bool>,
    abandon_on_restart: bool,
    context: &Context,
) -> DurableResult<TaskId> {
    let call = ToolCall {
        id: format!("{parent_call_id}/{key}"),
        name: name.to_owned(),
        arguments: args,
        ..ToolCall::default()
    };
    let (owner, conversation_id) = (runtime.task_id(), runtime.conversation_id());
    let (key, parent_call_id) = (key.to_owned(), parent_call_id.to_owned());
    let change = move |tx: Tx| async move {
        // Opened, or created empty for a key not made yet.
        let state = tx.family_doc(&NESTED_CALL_DOC, owner, &key, &()).await?;
        let existing = state.get()?.task_id;
        if let Some(existing) = existing {
            let record = tx.task(existing).await?;
            let replayed = record.as_ref().is_some_and(TaskRecord::is_terminal);
            let input = record.and_then(|record| record.input.deserialize_into::<ToolTaskInput>().ok());
            let same = matches!(&input, Some(ToolTaskInput::Nested { call: made, .. })
                if made.name == call.name && json_equal(&made.arguments, &call.arguments));
            if !same {
                let message = format!("Nested call {} was already made with another tool or other arguments", call.id);
                return Err(DurableError::error(message));
            }
            return Ok((existing, replayed));
        }
        let arguments =
            Json::from(serde_json::Value::Object(call.arguments.clone())).as_object().cloned().unwrap_or_default();
        let slot = NestedToolSlot {
            call_id: call.id.clone(),
            parent_call_id: parent_call_id.clone(),
            parent_task_id: owner,
            name: call.name.clone(),
            task_id: TaskId(0),
            arguments,
            status: SlotStatus::Pending,
            output: None,
            dropped_bytes: None,
            dropped_lines: None,
            details: None,
            diagnostics: None,
            summary: None,
        };
        let progress = progress.filter(|progress| !progress);
        let input = ToolTaskInput::Nested { parent: owner, parent_call_id, key: key.clone(), call, progress };
        let options = TaskOptions {
            ownership: TaskOwnership::Task(owner),
            conversation_id: None,
            background: false,
            abandon_on_restart,
            trace: Default::default(),
            links: Default::default(),
        };
        let created = tx.create_task(tool_task(), encode(&input)?, options).await?;
        state.edit(|state| state.task_id = Some(created))?;
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        live.edit(|live| {
            live.nested_tools.get_or_insert_with(Vec::new).push(NestedToolSlot { task_id: created, ..slot })
        })?;
        Ok((created, false))
    };
    let (id, replayed) = runtime.commit_map(change, context).await?;
    if replayed {
        context.telemetry().add_event("pi.durable.replay", SpanAttributes::new().with("tool.name", name));
    }
    Ok(id)
}

struct Written {
    text: String,
    details: Option<Json>,
    diagnostics: usize,
}

/// Throttled commits of what the tool reported into its slot, each writing only what changed since the last.
pub(crate) fn publish_progress(runtime: &TaskRuntime, state: Arc<CallState>, enabled: bool) -> Progress {
    let written = Arc::new(Mutex::new(Written { text: String::new(), details: None, diagnostics: 0 }));
    let (writer, reporter) = (runtime.clone(), runtime.clone());
    let interval = runtime.settings().progress.output_interval_ms;
    let (clock, timers) = (runtime.clone(), runtime.clone());
    let clock = ProgressClock {
        now: Arc::new(move || clock.now()),
        set_timer: Arc::new(move |callback, delay_ms| timers.set_timer(callback, delay_ms)),
    };
    Progress::with_clock(
        move || {
            let (runtime, state, written) = (writer.clone(), state.clone(), written.clone());
            Box::pin(async move {
                if !enabled {
                    return Ok(0);
                }
                // Capture everything now: the tool keeps reporting while the commit is in flight.
                let (snapshot, details, added, count) = {
                    let mut reported = lock(&state.reported);
                    let snapshot = reported.output.snapshot();
                    let seen = lock(&written).diagnostics;
                    let added: Vec<ToolDiagnostic> = reported.diagnostics[seen..].to_vec();
                    (snapshot, reported.details.clone(), added, reported.diagnostics.len())
                };
                let (bytes, details_changed) = {
                    let last = lock(&written);
                    let mut bytes = 0;
                    if snapshot.text != last.text {
                        let shared = if snapshot.text.starts_with(&last.text) {
                            last.text.len()
                        } else {
                            pi_chord::delta::overlap(&last.text, &snapshot.text, 65_536)
                        };
                        let shared = if snapshot.text.is_char_boundary(shared) { shared } else { 0 };
                        bytes += snapshot.text.len() - shared;
                    }
                    let changed = details != last.details;
                    if changed {
                        bytes += details.as_ref().map_or(4, |d| d.to_string().len());
                    }
                    if !added.is_empty() {
                        bytes += encode(&added)?.to_string().len();
                    }
                    (bytes, changed)
                };
                let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
                let (text, dropped) = (snapshot.text.clone(), (snapshot.dropped_bytes, snapshot.dropped_lines));
                let publish_details = if details_changed { details.clone() } else { None };
                let change = move |tx: Tx| async move {
                    tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
                        if let Some(mut slot) = tool_slot(live, task_id) {
                            slot.publish(&text, dropped, publish_details.as_ref(), &added);
                        }
                    })
                };
                runtime.commit_map_with_options(change, &runtime.invocation.context(), PROGRESS_COMMIT).await?;
                *lock(&written) = Written { text: snapshot.text, details, diagnostics: count };
                Ok(bytes as u64)
            })
        },
        move |error| {
            // Failures after an abort mark or close are expected; the committed state stays consistent.
            if !reporter.signal().aborted() {
                reporter.report(error);
            }
        },
        interval,
        clock,
    )
}
