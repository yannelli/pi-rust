//! Built-in generation task (upstream `harness/generation.ts`): prepares the positional system prompt and tool
//! loadout, requests or polls the model, retries, and classifies the response. The run's inputs live in
//! `pi.live.run`.

mod classify;
mod round;

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use pi_ai::{AssistantMessage, DeferredHandle, Message, ThinkingLevel, ToolCall};
use pi_chord::{Context, Json};
use serde::{Deserialize, Serialize};

use super::compaction::{CompactionInput, create_compaction, estimate_context, select_cut};
use super::define::PromptInput;
use super::live::{LIVE_DOC, LiveGeneration, LiveState, end_run};
use super::prompt::{plan_system_entries, render_sections, replay_sections};
use super::scheduler::{HookApi, TaskRuntime};
use super::settings::{CompactionPolicy, ConversationStreamOptions};
use super::task::{Task, TaskDefinition, define_task};
use super::types::{CompactionReason, ContextView, ModelRef, UserInput};
use super::usage::{UsageBucket, record_usage};
use crate::documents::encode;
use crate::entries::{ASSISTANT_ENTRY, TypedEntryDraft};
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId, SubmissionId, TaskId};
use crate::session::{Draft, Tx};
use crate::tasks::TaskOptions;
use crate::types::{
    EntryDraft, EntryQuery, EntryRecord, JoinPolicy, SubmissionSettlement, TaskOutcome, TaskOutcomeError, TaskRecord,
    TaskState,
};

/// Checkpoint of a generation task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum GenerationCheckpoint {
    Prepare {
        attempt: u32,
        /// The blocking compaction this generation waited for; it starts no other compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Error text of the overflow that started `compacted`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
    },
    Request {
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        thinking_level: ThinkingLevel,
        /// The settings' request options when preparation committed.
        stream_options: ConversationStreamOptions,
        /// Newest entry included in the request.
        cutoff: EntryId,
    },
    Retry {
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        until: f64,
    },
    Poll {
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        cutoff: EntryId,
        handle: DeferredHandle,
        poll_at: f64,
    },
    /// Waiting on the round's tool tasks, which the generation owns.
    Tools {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Tool tasks created so far, in call order.
        tools: Vec<TaskId>,
        /// Calls of a sequential round not started yet, in call order.
        pending: Vec<String>,
    },
}

/// Result of a generation task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationResult {
    pub entry_id: EntryId,
}

type Hook<A, R> = Arc<dyn Fn(A, HookApi, Context) -> BoxFuture<'static, DurableResult<R>> + Send + Sync>;

/// Hooks of the built-in generation task.
#[derive(Clone, Default)]
pub struct GenerationHooks {
    /// Before every request, including recovery; the result is used for that request only.
    pub before_request: Option<Hook<Vec<Message>, Option<Vec<Message>>>>,
    /// Every terminal provider message, before classification.
    pub after_response: Option<Hook<AssistantMessage, ()>>,
    /// A final answer; the first continuation appends a user message and continues the run.
    pub on_yield: Option<Hook<AssistantMessage, Option<UserInput>>>,
    /// After every tool of the round is terminal; results are the round's result entries in call order.
    pub after_tools: Option<Hook<(EntryId, Vec<EntryId>), ()>>,
}

pub(crate) fn decode(task: &TaskRecord) -> DurableResult<GenerationCheckpoint> {
    let checkpoint = task.state.checkpoint().cloned().unwrap_or(Json::Null);
    Ok(checkpoint.deserialize_into()?)
}

pub(crate) fn running(checkpoint: &GenerationCheckpoint) -> DurableResult<Option<TaskState>> {
    Ok(Some(TaskState::Running { checkpoint: encode(checkpoint)? }))
}

pub(crate) fn sleeping(checkpoint: &GenerationCheckpoint, until: f64) -> DurableResult<Option<TaskState>> {
    Ok(Some(TaskState::Sleeping { checkpoint: encode(checkpoint)?, until }))
}

pub(crate) fn waiting(checkpoint: &GenerationCheckpoint, on: Vec<TaskId>) -> DurableResult<Option<TaskState>> {
    Ok(Some(TaskState::Waiting { checkpoint: encode(checkpoint)?, on, policy: JoinPolicy::AllSettled }))
}

pub(crate) fn completed(result: Json) -> Option<TaskState> {
    Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(result) } })
}

pub(crate) fn failed(message: &str, reason: &str) -> Option<TaskState> {
    let mut detail = pi_chord::JsonMap::new();
    detail.insert("reason", reason);
    let error = TaskOutcomeError { message: message.to_owned(), detail: Some(Json::from(detail)) };
    Some(TaskState::Terminal { outcome: TaskOutcome::Failed { error, result: None } })
}

pub(crate) fn unanswered(reason: &str, detail: Option<&str>) -> SubmissionSettlement {
    SubmissionSettlement::Unanswered { reason: reason.into(), detail: detail.map(Json::from) }
}

static GENERATION_TASK: LazyLock<Task> = LazyLock::new(|| {
    let first = GenerationCheckpoint::Prepare { attempt: 1, compacted: None, overflow: None };
    let definition = TaskDefinition::new("pi.generation", 1, move |_| encode(&first))
        .phase("prepare", prepare)
        .phase("request", classify::request)
        .phase("retry", retry)
        .phase("poll", classify::poll)
        .phase("tools", round::tools_phase)
        .abort(round::abort);
    define_task(definition)
});

/// The built-in `pi.generation` task.
pub fn generation_task() -> Task {
    GENERATION_TASK.clone()
}

/// Render the system prompt and tool loadout, append the `pi.system` entries they need, and move to `request`.
async fn prepare(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let conversation_id = runtime.conversation_id();
    let agent = runtime.agent(&context).await?;
    let settings = runtime.settings();
    let model_ref = agent.model.clone();
    let resolved = model_ref.as_ref().and_then(|m| runtime.models().get_model(&m.provider, &m.model_id));
    let (Some(model_ref), Some(resolved)) = (model_ref.clone(), resolved) else {
        return fail_no_model(&runtime, model_ref.as_ref(), &context).await;
    };
    let GenerationCheckpoint::Prepare { attempt, compacted, overflow } = decode(&task)? else {
        return Err(DurableError::error("Generation checkpoint is not in phase prepare"));
    };
    if let (Some(compacted), Some(overflow)) = (compacted, &overflow) {
        let outcomes = runtime.outcomes(&[compacted], &context).await?;
        let summarized = match outcomes.first() {
            Some(TaskOutcome::Completed { result: Some(result) }) => result.get("entryId").is_some(),
            _ => false,
        };
        if !summarized {
            return fail_model_error(&runtime, overflow, &context).await;
        }
    }
    let view = runtime.context(conversation_id, &context, None).await?;
    let shown = replay_sections(&view.messages);
    let report = |error: &DurableError| runtime.report(error);
    let env = match runtime.env(&context).await {
        Ok(env) => env,
        Err(error) => {
            if context.is_cancelled() {
                return Err(error);
            }
            report(&error);
            None
        }
    };
    let input = PromptInput {
        conversation_id,
        agent: agent.clone(),
        env,
        shown: shown.iter().cloned().collect(),
        read: runtime.scheduler.core.session.clone(),
    };
    let wanted = render_sections(&agent.sections, &input, &shown, &report, &context).await?;
    let tools: Vec<_> = agent.tools.iter().map(|tool| tool.tool.clone()).collect();
    let entries = plan_system_entries(&view, &wanted, &tools, runtime.now());
    let threshold = match compacted {
        None => threshold_compaction(&view, &entries, resolved.context_window, &settings.compaction_for(&model_ref)),
        Some(_) => None,
    };
    let owner = runtime.task_id();
    if threshold == Some(Threshold::Blocking) {
        // Compact first and prepare again; the transcript is unchanged until the compaction appends.
        let change = move |tx: Tx, _| async move {
            let input = CompactionInput { reason: CompactionReason::Threshold, instructions: None };
            let child = create_compaction(&tx, conversation_id, input, Some(owner)).await?;
            let checkpoint = GenerationCheckpoint::Prepare { attempt, compacted: Some(child), overflow: None };
            waiting(&checkpoint, vec![child])
        };
        return runtime.commit(change, &context).await;
    }
    let thinking_level = agent.thinking_level;
    let stream_options = settings.stream.clone();
    let change = move |tx: Tx, _| async move {
        let page = tx.scan_entries(EntryQuery::new(conversation_id), 1, None).await?;
        let mut cutoff = page.items.first().map(|entry| entry.id);
        for entry in entries {
            cutoff = Some(tx.append_entry(conversation_id, entry).await?.id);
        }
        let Some(cutoff) = cutoff else {
            return Err(DurableError::error(format!("Conversation {conversation_id} has no entries to send")));
        };
        // Checked in this commit, so a compaction admitted during preparation counts.
        let background = threshold == Some(Threshold::Background);
        if background && tx.doc(&LIVE_DOC, conversation_id).await?.get()?.compactions.is_none() {
            let input = CompactionInput { reason: CompactionReason::Threshold, instructions: None };
            create_compaction(&tx, conversation_id, input, None).await?;
        }
        let model = model_ref;
        running(&GenerationCheckpoint::Request { attempt, compacted, model, thinking_level, stream_options, cutoff })
    };
    runtime.commit(change, &context).await
}

/// After the backoff's sleep; a run before `until` (an early wake, or a record of an earlier version) sleeps again.
async fn retry(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let GenerationCheckpoint::Retry { attempt, compacted, until } = decode(&task)? else {
        return Err(DurableError::error("Generation checkpoint is not in phase retry"));
    };
    if until > runtime.now() {
        let checkpoint = task.state.checkpoint().cloned().unwrap_or_default();
        return runtime.sleep_until(until, checkpoint, &context).await;
    }
    let conversation_id = runtime.conversation_id();
    let change = move |tx: Tx, _| async move {
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        live.edit(|live| live.generation = Some(LiveGeneration::new(attempt + 1)))?;
        running(&GenerationCheckpoint::Prepare { attempt: attempt + 1, compacted, overflow: None })
    };
    runtime.commit(change, &context).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Threshold {
    Blocking,
    Background,
}

/// Which threshold compaction preparation starts before its request (spec section 8.3).
fn threshold_compaction(
    view: &ContextView,
    planned: &[EntryDraft],
    context_window: u64,
    policy: &CompactionPolicy,
) -> Option<Threshold> {
    if !policy.enabled || context_window == 0 {
        return None;
    }
    let extra: Vec<Message> = planned.iter().flat_map(|entry| entry.model.clone().unwrap_or_default()).collect();
    let tokens = estimate_context(view, &extra) as i64;
    let blocking = context_window as i64 - policy.reserve_tokens as i64;
    let background = blocking - policy.background_tokens as i64;
    let over = if tokens > blocking {
        Threshold::Blocking
    } else if policy.background_tokens > 0 && tokens > background {
        Threshold::Background
    } else {
        return None;
    };
    select_cut(view, policy.keep_recent_tokens).map(|_| over)
}

/// Settle the run's inputs `unanswered` with `model_error` and fail with `text`.
pub(crate) async fn fail_model_error(runtime: &TaskRuntime, text: &str, context: &Context) -> DurableResult<()> {
    let (task_id, conversation_id, text) = (runtime.task_id(), runtime.conversation_id(), text.to_owned());
    let change = move |tx: Tx, _| async move {
        let settlement = unanswered("model_error", Some(&text));
        edit_live(&tx, conversation_id, |tx, live| end_run(tx, live, task_id, settlement)).await?;
        Ok(failed(&text, "model_error"))
    };
    runtime.commit(change, context).await
}

/// Settle the run's inputs `unanswered` with `no_model` and fail.
pub(crate) async fn fail_no_model(
    runtime: &TaskRuntime,
    model: Option<&ModelRef>,
    context: &Context,
) -> DurableResult<()> {
    let message = match model {
        None => "No model is configured".to_owned(),
        Some(m) => format!("Model {}/{} is not available", m.provider, m.model_id),
    };
    let (task_id, conversation_id) = (runtime.task_id(), runtime.conversation_id());
    let change = move |tx: Tx, _| async move {
        edit_live(&tx, conversation_id, |tx, live| end_run(tx, live, task_id, unanswered("no_model", None))).await?;
        Ok(failed(&message, "no_model"))
    };
    runtime.commit(change, context).await
}

/// Read, change, and write back `pi.live` of a conversation.
pub(crate) async fn edit_live<R>(
    tx: &Tx,
    conversation_id: ConversationId,
    change: impl FnOnce(&Tx, &mut LiveState) -> DurableResult<R>,
) -> DurableResult<R> {
    let draft: Draft<LiveState> = tx.doc(&LIVE_DOC, conversation_id).await?;
    let mut live = draft.get()?;
    let result = change(tx, &mut live)?;
    draft.set(&live)?;
    Ok(result)
}

/// Append a committed partial left by an interrupted request as an aborted assistant entry.
pub(crate) async fn convert_partial(
    tx: &Tx,
    live: &mut LiveState,
    conversation_id: ConversationId,
) -> DurableResult<()> {
    let Some(partial) = live.generation.as_ref().and_then(|generation| generation.message.clone()) else {
        return Ok(());
    };
    let Message::Assistant(mut message) = partial.deserialize_into()? else {
        return Err(DurableError::error("Committed partial is not an assistant message"));
    };
    message.stop_reason = pi_ai::StopReason::Aborted;
    append_assistant(tx, conversation_id, &message).await?;
    Ok(())
}

/// Append a provider result and add its usage to `pi.usage` in the same commit.
pub(crate) async fn append_assistant(
    tx: &Tx,
    conversation_id: ConversationId,
    message: &AssistantMessage,
) -> DurableResult<EntryRecord> {
    let key = format!("{}/{}", message.provider, message.model);
    record_usage(tx, conversation_id, UsageBucket::Models, &key, &message.usage).await?;
    let draft = TypedEntryDraft { model: Some(vec![Message::Assistant(message.clone())]), ..TypedEntryDraft::new(()) };
    Ok(tx.append_typed_entry(&ASSISTANT_ENTRY, conversation_id, draft).await?.record)
}

/// Start a run for `inputs`, placed input submissions: a new generation takes `pi.live.run`.
pub(crate) async fn start_run(
    tx: &Tx,
    conversation_id: ConversationId,
    live: &Draft<LiveState>,
    inputs: Vec<SubmissionId>,
) -> DurableResult<()> {
    let task_id = create_generation(tx, conversation_id).await?;
    live.edit(|live| live.run = Some(super::live::LiveRun { task_id, inputs }))
}

/// A generation owned by its conversation.
pub(crate) async fn create_generation(tx: &Tx, conversation_id: ConversationId) -> DurableResult<TaskId> {
    tx.create_task(generation_task(), Json::empty_object(), TaskOptions::conversation(conversation_id)).await
}

/// Hand run control from `from` to `to`; the run's inputs move with it.
pub(crate) fn hand_over(live: &mut LiveState, from: TaskId, to: TaskId) {
    if let Some(run) = live.run.as_mut().filter(|run| run.task_id == from) {
        run.task_id = to;
    }
}

/// The calls `call_ids` of the assistant entry, in the given order.
pub(crate) fn calls_of(entry: Option<&EntryRecord>, call_ids: &[String]) -> Vec<ToolCall> {
    let calls: Vec<ToolCall> = match entry.and_then(|entry| entry.model.as_ref()).and_then(|model| model.first()) {
        Some(Message::Assistant(message)) => message.tool_calls().cloned().collect(),
        _ => Vec::new(),
    };
    call_ids.iter().filter_map(|id| calls.iter().find(|call| call.id == *id).cloned()).collect()
}
