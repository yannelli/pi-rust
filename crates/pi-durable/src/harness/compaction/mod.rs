//! Built-in compaction task (upstream `harness/compaction.ts`, spec section 8.7): select an old prefix of the
//! model context, summarize it, and place a summary entry whose `head` is the first kept entry.

mod select;

use std::sync::{Arc, LazyLock};

use futures::future::BoxFuture;
use pi_ai::retry::{is_retryable_assistant_error, retry_delay_ms};
use pi_ai::{
    AssistantMessage, ContentBlock, Message, StopReason, SystemMessage, ThinkingLevel, UserContent, UserMessage,
};
use pi_chord::{Context, Json, JsonMap};
use serde::{Deserialize, Serialize};

pub use select::{estimate_context, select_cut, serialize_conversation, summarized_messages};

use super::live::{
    CompactionStatus, LIVE_DOC, LiveState, add_compaction_status, compaction_status, remove_compaction_status,
};
use super::scheduler::{HookApi, TaskRuntime};
use super::settings::ConversationStreamOptions;
use super::submissions::admit_submission;
use super::task::{Task, TaskDefinition, define_task};
use super::types::{CompactionReason, CompactionResult, ModelRef, SubmissionDraft};
use super::usage::{UsageBucket, ensure_provider_session_id, record_usage};
use crate::documents::encode;
use crate::entries::COMPACTION_ENTRY;
use crate::errors::{DurableError, DurableResult};
use crate::ids::{ConversationId, EntryId, TaskId};
use crate::models::SimpleStreamOptions;
use crate::session::Tx;
use crate::tasks::{TaskOptions, TaskOwnership};
use crate::types::{DraftHead, EntryDraft, EntryRecord, TaskOutcome, TaskOutcomeError, TaskRecord, TaskState};

/// Input of a compaction task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionInput {
    pub reason: CompactionReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The pinned summarization request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryRequest {
    pub attempt: u32,
    pub model: ModelRef,
    pub thinking_level: ThinkingLevel,
    pub stream_options: ConversationStreamOptions,
    pub max_tokens: u64,
    /// Newest entry of the context the range was selected from.
    pub tail: EntryId,
    /// First entry kept verbatim; the summary's `head`.
    pub first_kept: EntryId,
}

/// Checkpoint of a compaction task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum CompactionCheckpoint {
    Select,
    Summarize(SummaryRequest),
    Retry {
        until: f64,
        #[serde(flatten)]
        request: SummaryRequest,
    },
}

/// What `before_compact` sees: the entries the summary replaces, their messages, and the first kept entry.
#[derive(Clone, Debug)]
pub struct CompactionSelection {
    pub reason: CompactionReason,
    pub entries: Vec<EntryRecord>,
    pub messages: Vec<Message>,
    pub first_kept: EntryId,
    pub instructions: Option<String>,
}

/// What `before_compact` decides: decline, or a summary to place without a model request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompactionDecision {
    Decline,
    Summary(String),
}

type Hook<A, R> = Arc<dyn Fn(A, HookApi, Context) -> BoxFuture<'static, DurableResult<R>> + Send + Sync>;

/// Hooks of the built-in compaction task.
#[derive(Clone, Default)]
pub struct CompactionHooks {
    /// After range selection, before summarizing; the first decision wins.
    pub before_compact: Option<Hook<CompactionSelection, Option<CompactionDecision>>>,
}

const SUMMARY_PREFIX: &str =
    "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
const SUMMARY_SUFFIX: &str = "\n</summary>";

const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = include_str!("summarization_prompt.txt");

static COMPACTION_TASK: LazyLock<Task> = LazyLock::new(|| {
    let definition = TaskDefinition::new("pi.compaction", 1, |_| encode(&CompactionCheckpoint::Select))
        .phase("select", select_phase)
        .phase("summarize", summarize_phase)
        .phase("retry", retry_phase)
        .abort(|_, runtime, context| async move { finish(&runtime, &context, None).await });
    define_task(definition)
});

/// The built-in `pi.compaction` task.
pub fn compaction_task() -> Task {
    COMPACTION_TASK.clone()
}

fn checkpoint(task: &TaskRecord) -> DurableResult<CompactionCheckpoint> {
    Ok(task.state.checkpoint().cloned().unwrap_or(Json::Null).deserialize_into()?)
}

async fn select_phase(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let conversation_id = runtime.conversation_id();
    let agent = runtime.agent(&context).await?;
    let settings = runtime.settings();
    let model = agent.model.as_ref().and_then(|m| runtime.models().get_model(&m.provider, &m.model_id));
    let (Some(model_ref), Some(model)) = (agent.model.clone(), model) else {
        return fail_no_model(&runtime, agent.model.as_ref(), &context).await;
    };
    let policy = settings.compaction;
    let view = runtime.context(conversation_id, &context, None).await?;
    let Some(cut) = select_cut(&view, policy.keep_recent_tokens) else {
        return finish(&runtime, &context, Some(Json::empty_object())).await;
    };
    let first_kept = view.entries[cut].id;
    let input: CompactionInput = task.input.deserialize_into()?;
    let selection = CompactionSelection {
        reason: input.reason,
        entries: view.entries[..cut].to_vec(),
        messages: summarized_messages(&view, cut),
        first_kept,
        instructions: input.instructions.clone(),
    };
    let mut decision = None;
    for hooks in runtime.hooks::<CompactionHooks>(&context).await? {
        let Some(hook) = &hooks.before_compact else { continue };
        if decision.is_some() {
            break;
        }
        match hook(selection.clone(), runtime.clone(), context.clone()).await {
            Ok(next) => decision = next,
            Err(error) => runtime.hook_failed(error)?,
        }
    }
    match decision {
        Some(CompactionDecision::Decline) => return finish(&runtime, &context, Some(Json::empty_object())).await,
        Some(CompactionDecision::Summary(summary)) => return place(&runtime, first_kept, summary, &context).await,
        None => {}
    }
    let model_max = if model.max_tokens > 0 { model.max_tokens } else { u64::MAX };
    let tail = view.entries.iter().map(|entry| entry.id).fold(first_kept, EntryId::max);
    let request = SummaryRequest {
        attempt: 1,
        model: model_ref,
        thinking_level: agent.thinking_level,
        stream_options: settings.stream,
        max_tokens: ((policy.reserve_tokens as f64 * 0.8).floor() as u64).min(model_max),
        tail,
        first_kept,
    };
    let next = encode(&CompactionCheckpoint::Summarize(request))?;
    runtime.commit(move |_, _| async move { Ok(Some(TaskState::Running { checkpoint: next })) }, &context).await
}

async fn summarize_phase(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let CompactionCheckpoint::Summarize(request) = checkpoint(&task)? else {
        return Err(DurableError::error("Compaction checkpoint is not in phase summarize"));
    };
    let Some(model) = runtime.models().get_model(&request.model.provider, &request.model.model_id) else {
        return fail_no_model(&runtime, Some(&request.model), &context).await;
    };
    // The context at `tail` is immutable, so this is the range `select` chose.
    let view = runtime.context(runtime.conversation_id(), &context, Some(request.tail)).await?;
    let cut = view.entries.iter().position(|entry| entry.id == request.first_kept).unwrap_or(view.entries.len());
    let input: CompactionInput = task.input.deserialize_into()?;
    let now = runtime.now() as i64;
    let prompt = summary_prompt(&summarized_messages(&view, cut), input.instructions.as_deref());
    let messages = vec![
        Message::System(SystemMessage {
            content: UserContent::Text(SUMMARIZATION_SYSTEM_PROMPT.into()),
            sections: None,
            tools_added: None,
            tools_removed: None,
            timestamp: now,
        }),
        Message::User(UserMessage { content: UserContent::Blocks(vec![ContentBlock::text(prompt)]), timestamp: now }),
    ];
    let stream = &request.stream_options;
    let options = SimpleStreamOptions {
        signal: Some(runtime.signal()),
        headers: stream.headers.clone().unwrap_or_default(),
        timeout_ms: stream.timeout_ms,
        max_retries: stream.max_retries,
        max_retry_delay_ms: stream.max_retry_delay_ms,
        transport: stream.transport,
        metadata: stream.metadata.as_ref().and_then(|m| Json::from(m.clone()).to_serde().as_object().cloned()),
        cache_retention: Some(pi_ai::CacheRetention::None),
        max_tokens: Some(request.max_tokens),
        session_id: Some(ensure_provider_session_id(&runtime, &context).await?),
        reasoning: (request.thinking_level != ThinkingLevel::Off).then_some(request.thinking_level),
        ..SimpleStreamOptions::default()
    };
    let context_request = pi_ai::Context { system_prompt: None, messages, tools: Vec::new() };
    let message = runtime.models().complete_simple(&model, &context_request, options).await;
    // An abort mark or close: the abort invocation or the reopened task handles the committed state.
    runtime.signal().throw_if_aborted().map_err(DurableError::from)?;
    let summary = summary_text(&message);
    let policy = runtime.settings().retry;
    let retry = message.stop_reason == StopReason::Error
        && is_retryable_assistant_error(&message)
        && policy.enabled
        && request.attempt <= policy.max_retries;
    let until = if retry {
        runtime.now() + retry_delay_ms(policy.base_delay_ms, policy.max_agent_delay_ms, request.attempt) as f64
    } else {
        0.0
    };
    let (conversation_id, task_id, runtime2) = (runtime.conversation_id(), runtime.task_id(), runtime.clone());
    let change = move |tx: Tx, current: TaskRecord| async move {
        let key = format!("{}/{}", message.provider, message.model);
        record_usage(&tx, conversation_id, UsageBucket::Models, &key, &message.usage).await?;
        if let Some(summary) = summary {
            return place_summary(&tx, &runtime2, &current, request.first_kept, &summary).await;
        }
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        if retry {
            let error = message.error_message.clone().unwrap_or_default();
            live.edit(|live| {
                if let Some(status) = compaction_status(live, task_id) {
                    status.retry = Some(super::live::RetryState { at: until, error });
                }
            })?;
            let next = encode(&CompactionCheckpoint::Retry { until, request })?;
            return Ok(Some(TaskState::Running { checkpoint: next }));
        }
        live.edit(|live| remove_compaction_status(live, task_id))?;
        Ok(failed(&summary_failure(&message), "model_error"))
    };
    runtime.commit(change, &context).await
}

async fn retry_phase(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let CompactionCheckpoint::Retry { until, request } = checkpoint(&task)? else {
        return Err(DurableError::error("Compaction checkpoint is not in phase retry"));
    };
    runtime.sleep(until, &context).await?;
    let attempt = request.attempt + 1;
    let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
    let change = move |tx: Tx, _| async move {
        tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
            if let Some(status) = compaction_status(live, task_id) {
                status.attempt = attempt;
                status.retry = None;
            }
        })?;
        let next = encode(&CompactionCheckpoint::Summarize(SummaryRequest { attempt, ..request }))?;
        Ok(Some(TaskState::Running { checkpoint: next }))
    };
    runtime.commit(change, &context).await
}

/// Create a compaction task with its status in this commit. `owner` is the generation that waits for it (a
/// blocking compaction); without one it is conversation-owned, and background unless manual.
pub async fn create_compaction(
    tx: &Tx,
    conversation_id: ConversationId,
    input: CompactionInput,
    owner: Option<TaskId>,
) -> DurableResult<TaskId> {
    let ownership = owner.map_or(TaskOwnership::Conversation, TaskOwnership::Task);
    let background = owner.is_none() && input.reason != CompactionReason::Manual;
    let conversation = owner.is_none().then_some(conversation_id);
    let options = TaskOptions { ownership, conversation_id: conversation, background, abandon_on_restart: false };
    let reason = input.reason;
    let task_id = tx.create_task(compaction_task(), encode(&input)?, options).await?;
    let status = CompactionStatus { task_id, reason, blocking: owner.is_some(), attempt: 1, retry: None };
    tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| add_compaction_status(live, status))?;
    Ok(task_id)
}

/// The summary of a clean `stop` with text and no tool call; anything else is not a summary.
fn summary_text(message: &AssistantMessage) -> Option<String> {
    if message.stop_reason != StopReason::Stop || message.tool_calls().next().is_some() {
        return None;
    }
    let text = message.content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

fn summary_failure(message: &AssistantMessage) -> String {
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            let reason = if message.stop_reason == StopReason::Error { "error" } else { "aborted" };
            format!("Summarization failed: {}", message.error_message.clone().unwrap_or_else(|| reason.into()))
        }
        StopReason::Length => "Summarization hit the token limit; the summary is incomplete".into(),
        _ if message.tool_calls().next().is_some() => "Summarization attempted to call a tool".into(),
        _ => "Summarization produced no text".into(),
    }
}

fn summary_prompt(messages: &[Message], instructions: Option<&str>) -> String {
    let focus = instructions.map(|text| format!("\n\nAdditional focus: {text}")).unwrap_or_default();
    format!("<conversation>\n{}\n</conversation>\n\n{SUMMARIZATION_PROMPT}{focus}", serialize_conversation(messages))
}

fn failed(message: &str, reason: &str) -> Option<TaskState> {
    let mut detail = JsonMap::new();
    detail.insert("reason", reason);
    let error = TaskOutcomeError { message: message.to_owned(), detail: Some(Json::from(detail)) };
    Some(TaskState::Terminal { outcome: TaskOutcome::Failed { error, result: None } })
}

/// Place a summary supplied by a hook in its own commit.
async fn place(runtime: &TaskRuntime, first_kept: EntryId, summary: String, context: &Context) -> DurableResult<()> {
    let runtime2 = runtime.clone();
    let change = move |tx: Tx, current: TaskRecord| async move {
        place_summary(&tx, &runtime2, &current, first_kept, &summary).await
    };
    runtime.commit(change, context).await
}

/// Place the summary entry and complete. A blocking compaction appends it; a conversation-owned one admits it
/// as a write submission, placed at once when idle, otherwise at the next boundary.
async fn place_summary(
    tx: &Tx,
    runtime: &TaskRuntime,
    current: &TaskRecord,
    first_kept: EntryId,
    summary: &str,
) -> DurableResult<Option<TaskState>> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live: &mut LiveState| remove_compaction_status(live, task_id))?;
    let text = format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}");
    let input: CompactionInput = current.input.deserialize_into()?;
    let mut data = JsonMap::new();
    data.insert("reason", input.reason.as_str());
    let message =
        UserMessage { content: UserContent::Blocks(vec![ContentBlock::text(text)]), timestamp: runtime.now() as i64 };
    let entry = EntryDraft {
        model: Some(vec![Message::User(message)]),
        data: Some(Json::from(data)),
        head: Some(DraftHead::Entry(first_kept)),
        ..EntryDraft::new(COMPACTION_ENTRY.kind())
    };
    let result = match current.owner {
        None => {
            let draft = SubmissionDraft::Write { request_id: Some(format!("compaction:{task_id}")), entry };
            let core = &runtime.scheduler.core;
            let id = admit_submission(tx, conversation_id, draft, core.now(), &core.settings()).await?;
            CompactionResult { submission_id: Some(id), entry_id: None }
        }
        Some(_) => {
            CompactionResult { entry_id: Some(tx.append_entry(conversation_id, entry).await?.id), submission_id: None }
        }
    };
    Ok(Some(TaskState::Terminal { outcome: TaskOutcome::Completed { result: Some(encode(&result)?) } }))
}

/// Remove the status and end: completed with `result`, or aborted without one.
async fn finish(runtime: &TaskRuntime, context: &Context, result: Option<Json>) -> DurableResult<()> {
    let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
    let change = move |tx: Tx, _| async move {
        tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| remove_compaction_status(live, task_id))?;
        let outcome = match result {
            Some(result) => TaskOutcome::Completed { result: Some(result) },
            None => TaskOutcome::Aborted { reason: None, result: None },
        };
        Ok(Some(TaskState::Terminal { outcome }))
    };
    runtime.commit(change, context).await
}

async fn fail_no_model(runtime: &TaskRuntime, model: Option<&ModelRef>, context: &Context) -> DurableResult<()> {
    let message = match model {
        None => "No model is configured".to_owned(),
        Some(m) => format!("Model {}/{} is not available", m.provider, m.model_id),
    };
    let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
    let change = move |tx: Tx, _| async move {
        tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| remove_compaction_status(live, task_id))?;
        Ok(failed(&message, "no_model"))
    };
    runtime.commit(change, context).await
}
