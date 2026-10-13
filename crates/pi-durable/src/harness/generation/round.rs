//! Tool rounds of a generation (spec section 8.5): starting the round, the `tools` phase, and the abort handler.

use pi_ai::transcript::current_tools;
use pi_ai::{AssistantMessage, Message, ToolCall, UserContent, UserMessage};
use pi_chord::{Context, Json, JsonMap};

use super::classify::Request;
use super::{
    GenerationCheckpoint, GenerationHooks, append_assistant, calls_of, completed, convert_partial, create_generation,
    decode, hand_over, unanswered, waiting,
};
use crate::documents::encode;
use crate::entries::{ASSISTANT_ENTRY, RESET_ENTRY, TypedEntryDraft};
use crate::errors::{DurableError, DurableResult};
use crate::harness::agent::add_tools;
use crate::harness::inbox::{BoundaryAt, apply_boundary, prepare_boundary};
use crate::harness::live::{LIVE_DOC, LiveState, SlotStatus, ToolSlot, end_run};
use crate::harness::scheduler::TaskRuntime;
use crate::harness::tool::{ToolTaskInput, append_tool_result, harness_error, tool_task};
use crate::harness::types::{ToolControl, ToolExecutionMode};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::models::DeferredCancelOptions;
use crate::session::Tx;
use crate::tasks::TaskOptions;
use crate::types::{DraftHead, SubmissionSettlement, TaskOutcome, TaskRecord, TaskState};

/// A tool task for call `call_id`, owned by the generation.
async fn create_tool_task(tx: &Tx, owner: TaskId, assistant: EntryId, call_id: String) -> DurableResult<TaskId> {
    let input = encode(&ToolTaskInput::Model { assistant, call_id })?;
    tx.create_task(tool_task(), input, TaskOptions::owned_by(owner)).await
}

/// Append the tool-calling answer and start its tool round in one commit (spec section 8.3).
pub(super) async fn start_tool_round(
    runtime: &TaskRuntime,
    request: &Request,
    message: AssistantMessage,
    calls: Vec<ToolCall>,
    context: &Context,
) -> DurableResult<()> {
    let conversation_id = runtime.conversation_id();
    let messages = match &request.messages {
        Some(messages) => messages.clone(),
        None => runtime.context(conversation_id, context, Some(request.cutoff)).await?.messages,
    };
    let offered: Vec<String> = current_tools(&messages).into_iter().map(|tool| tool.name).collect();
    // Read as the round starts; a tool is resolved as its tool task resolves it.
    let tools = runtime.agent(context).await?.tools;
    let sequential = runtime.settings().tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            offered.contains(&call.name)
                && tools.iter().find(|tool| tool.name() == call.name).and_then(|tool| tool.execution_mode)
                    == Some(ToolExecutionMode::Sequential)
        });
    let (owner, core) = (runtime.task_id(), runtime.scheduler.core.clone());
    let change = move |tx: Tx, _| async move {
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        let entry = append_assistant(&tx, conversation_id, &message).await?;
        let (mut slots, mut tasks, mut pending) = (Vec::new(), Vec::new(), Vec::new());
        for call in calls {
            if !offered.contains(&call.name) {
                let unavailable = harness_error("tool_unavailable", &format!("Tool {} is not available", call.name));
                let result = append_tool_result(&tx, conversation_id, &call, &unavailable, core.now(), None).await?;
                slots
                    .push(ToolSlot { entry: Some(result.id), ..ToolSlot::new(&call.id, &call.name, SlotStatus::Done) });
                continue;
            }
            if sequential && !tasks.is_empty() {
                slots.push(ToolSlot::new(&call.id, &call.name, SlotStatus::Pending));
                pending.push(call.id);
                continue;
            }
            let task_id = create_tool_task(&tx, owner, entry.id, call.id.clone()).await?;
            tasks.push(task_id);
            slots.push(ToolSlot { task_id: Some(task_id), ..ToolSlot::new(&call.id, &call.name, SlotStatus::Pending) });
        }
        live.edit(|live| {
            live.generation = None;
            live.tools = Some(slots);
        })?;
        let checkpoint = GenerationCheckpoint::Tools { assistant: entry.id, tools: tasks.clone(), pending };
        waiting(&checkpoint, tasks)
    };
    runtime.commit(change, context).await
}

pub(super) async fn tools_phase(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let GenerationCheckpoint::Tools { assistant, tools, pending } = decode(&task)? else {
        return Err(DurableError::error("Generation checkpoint is not in phase tools"));
    };
    let Some((next, rest)) = pending.split_first() else {
        return finish_tool_round(&runtime, assistant, tools, &context).await;
    };
    // Sequential round: start the next call and wait for it.
    let (next, rest, owner, conversation_id) =
        (next.clone(), rest.to_vec(), runtime.task_id(), runtime.conversation_id());
    let change = move |tx: Tx, _| async move {
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        let task_id = create_tool_task(&tx, owner, assistant, next.clone()).await?;
        live.edit(|live| {
            let slots = live.tools.iter_mut().flatten();
            if let Some(slot) = slots.into_iter().find(|slot| slot.call_id == next && slot.task_id.is_none()) {
                slot.task_id = Some(task_id);
            }
        })?;
        let mut tools = tools;
        tools.push(task_id);
        waiting(&GenerationCheckpoint::Tools { assistant, tools, pending: rest }, vec![task_id])
    };
    runtime.commit(change, &context).await
}

/// The round's tools are terminal: apply their controls and end the run at the final boundary, or hand it to
/// the next generation at the post-tools boundary.
async fn finish_tool_round(
    runtime: &TaskRuntime,
    assistant: EntryId,
    tools: Vec<TaskId>,
    context: &Context,
) -> DurableResult<()> {
    let conversation_id = runtime.conversation_id();
    let outcomes = runtime.outcomes(&tools, context).await?;
    let controls: Vec<(TaskId, Option<ToolControl>)> = tools
        .iter()
        .zip(outcomes)
        .map(|(id, outcome)| {
            let control = match outcome {
                TaskOutcome::Completed { result: Some(result) }
                    if result.get("kind").and_then(Json::as_str) != Some("nested") =>
                {
                    result.get("control").and_then(|control| control.deserialize_into::<ToolControl>().ok())
                }
                _ => None,
            };
            (*id, control)
        })
        .collect();
    let live: Option<LiveState> =
        runtime.snapshot(&LIVE_DOC, conversation_id, context).await?.map(|v| LIVE_DOC.decode(&v)).transpose()?;
    let slots = live.and_then(|live| live.tools).unwrap_or_default();
    let results: Vec<EntryId> = slots.iter().filter_map(|slot| slot.entry).collect();
    for hooks in runtime.hooks::<GenerationHooks>(context).await? {
        let Some(hook) = &hooks.after_tools else { continue };
        if let Err(error) = hook((assistant, results.clone()), runtime.clone(), context.clone()).await {
            runtime.hook_failed(error)?;
        }
    }
    let control_of = |id: TaskId| controls.iter().find(|(task, _)| *task == id).and_then(|(_, c)| c.clone());
    // Every call of the round, including those answered without a task, must ask to terminate.
    let terminate = !slots.is_empty()
        && slots.iter().all(|slot| slot.task_id.and_then(control_of).is_some_and(|c| c.terminate == Some(true)));
    let added: Vec<String> =
        controls.iter().flat_map(|(_, c)| c.iter().flat_map(|c| c.add_tools.clone().unwrap_or_default())).collect();
    // The last handoff in call order wins.
    let handoff = controls.iter().rev().find_map(|(_, c)| c.as_ref().and_then(|c| c.handoff.clone()));
    let (owner, core) = (runtime.task_id(), runtime.scheduler.core.clone());
    let change = move |tx: Tx, _| async move {
        let mut boundary = prepare_boundary(&tx, conversation_id, &core.settings()).await?;
        if !added.is_empty() {
            add_tools(&tx, conversation_id, &added).await?;
        }
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        let now = core.now();
        let done = SubmissionSettlement::Done { answer: assistant };
        if terminate || handoff.is_some() {
            if let Some(handoff) = handoff {
                let message = Message::User(UserMessage { content: UserContent::Text(handoff), timestamp: now as i64 });
                let draft = TypedEntryDraft {
                    model: Some(vec![message]),
                    head: Some(DraftHead::SelfEntry),
                    ..TypedEntryDraft::new(())
                };
                let entry = tx.append_typed_entry(&RESET_ENTRY, conversation_id, draft).await?;
                boundary.head = Some(entry.record.id);
            }
            let placed = apply_boundary(&tx, &mut boundary, BoundaryAt::Final, now).await?;
            end_live(&tx, &live, owner, done)?;
            if !placed.users.is_empty() {
                super::start_run(&tx, conversation_id, &live, placed.users).await?;
            }
        } else {
            let placed = apply_boundary(&tx, &mut boundary, BoundaryAt::PostTools, now).await?;
            if placed.reset {
                // The queued reset cut the run's context before an answer.
                end_live(&tx, &live, owner, unanswered("reset", None))?;
                if !placed.users.is_empty() {
                    super::start_run(&tx, conversation_id, &live, placed.users).await?;
                }
            } else {
                let successor = create_generation(&tx, conversation_id).await?;
                live.edit(|live| {
                    live.tools = None;
                    live.nested_tools = None;
                    if let Some(run) = live.run.as_mut().filter(|run| run.task_id == owner) {
                        run.inputs.extend(placed.users);
                    }
                    hand_over(live, owner, successor);
                })?;
            }
        }
        let mut result = JsonMap::new();
        result.insert("entryId", encode(&assistant)?);
        Ok(completed(Json::from(result)))
    };
    runtime.commit(change, context).await
}

fn end_live(
    tx: &Tx,
    live: &crate::session::Draft<LiveState>,
    owner: TaskId,
    settlement: SubmissionSettlement,
) -> DurableResult<()> {
    let mut state = live.get()?;
    end_run(tx, &mut state, owner, settlement)?;
    live.set(&state)
}

/// Runs after the round's tool tasks are terminal; calls never started get `aborted` results.
pub(super) async fn abort(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let checkpoint = decode(&task)?;
    if let GenerationCheckpoint::Poll { model, handle, .. } = &checkpoint
        && let Some(model) = runtime.models().get_model(&model.provider, &model.model_id)
    {
        let options = DeferredCancelOptions { signal: Some(runtime.signal()), ..DeferredCancelOptions::default() };
        if let Err(error) = runtime.models().cancel_deferred(&model, handle, options).await {
            runtime.report(&error);
        }
    }
    let unstarted = match &checkpoint {
        GenerationCheckpoint::Tools { assistant, pending, .. } => {
            let entry = runtime.typed_entry(&ASSISTANT_ENTRY, *assistant, &context).await?;
            calls_of(entry.as_ref().map(|entry| &entry.record), pending)
        }
        _ => Vec::new(),
    };
    let (owner, conversation_id, core) = (runtime.task_id(), runtime.conversation_id(), runtime.scheduler.core.clone());
    let change = move |tx: Tx, _| async move {
        abort_in(&tx, conversation_id, owner, unstarted, core.now()).await?;
        Ok(Some(TaskState::Terminal { outcome: TaskOutcome::Aborted { reason: None, result: None } }))
    };
    runtime.commit(change, &context).await
}

async fn abort_in(
    tx: &Tx,
    conversation_id: ConversationId,
    owner: TaskId,
    unstarted: Vec<ToolCall>,
    now: f64,
) -> DurableResult<()> {
    let draft = tx.doc(&LIVE_DOC, conversation_id).await?;
    let mut live = draft.get()?;
    convert_partial(tx, &mut live, conversation_id).await?;
    for call in unstarted {
        let result = harness_error("aborted", &format!("Tool {} was aborted", call.name));
        append_tool_result(tx, conversation_id, &call, &result, now, None).await?;
    }
    end_run(tx, &mut live, owner, unanswered("aborted", None))?;
    draft.set(&live)
}
