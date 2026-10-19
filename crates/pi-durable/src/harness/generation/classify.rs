//! The `request` and `poll` phases: streaming with throttled partial commits, and classification of the
//! terminal provider message.

use std::time::Duration;

use futures::StreamExt;
use pi_ai::retry::{is_retryable_assistant_error, retry_delay_ms};
use pi_ai::{AssistantMessage, AssistantMessageEvent, Message, Model, StopReason, is_context_overflow};
use pi_chord::{Context, Json};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    GenerationCheckpoint, GenerationHooks, append_assistant, completed, convert_partial, create_generation, decode,
    edit_live, fail_no_model, hand_over, running, unanswered, waiting,
};
use crate::documents::encode;
use crate::errors::{DurableError, DurableResult};
use crate::harness::compaction::{CompactionInput, create_compaction, select_cut};
use crate::harness::inbox::{BoundaryAt, append_user, apply_boundary, prepare_boundary};
use crate::harness::live::{DeferredPoll, LIVE_DOC, LiveGeneration, RetryState, end_run};
use crate::harness::scheduler::TaskRuntime;
use crate::harness::types::{CompactionReason, ModelRef, UserInput};
use crate::harness::usage::ensure_provider_session_id;
use crate::ids::{EntryId, TaskId};
use crate::models::{DeferredFetchOptions, SimpleStreamOptions};
use crate::session::Tx;
use crate::types::{SubmissionSettlement, TaskRecord};

const DEFAULT_POLL_AFTER_MS: f64 = 5000.0;

/// What classification needs from the request that produced a message.
pub(super) struct Request {
    pub attempt: u32,
    pub compacted: Option<TaskId>,
    pub model: ModelRef,
    pub cutoff: EntryId,
    /// Committed model context through `cutoff`, when the phase already derived it.
    pub messages: Option<Vec<Message>>,
    /// Set when the message came from polling, so a still deferred result polls strictly later.
    pub poll_at: Option<f64>,
}

pub(super) async fn request(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let GenerationCheckpoint::Request { attempt, compacted, model: model_ref, thinking_level, stream_options, cutoff } =
        decode(&task)?
    else {
        return Err(DurableError::error("Generation checkpoint is not in phase request"));
    };
    let conversation_id = runtime.conversation_id();
    let change = move |tx: Tx, _| async move {
        let draft = tx.doc(&LIVE_DOC, conversation_id).await?;
        let mut live = draft.get()?;
        convert_partial(&tx, &mut live, conversation_id).await?;
        live.generation = Some(LiveGeneration::new(attempt));
        draft.set(&live)?;
        Ok(None)
    };
    runtime.commit(change, &context).await?;
    let Some(model) = runtime.models().get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(&runtime, Some(&model_ref), &context).await;
    };
    let view = runtime.context(conversation_id, &context, Some(cutoff)).await?;
    let mut messages = view.messages.clone();
    for hooks in runtime.hooks::<GenerationHooks>(&context).await? {
        let Some(hook) = &hooks.before_request else { continue };
        match hook(messages.clone(), runtime.clone(), context.clone()).await {
            Ok(Some(replaced)) => messages = replaced,
            Ok(None) => {}
            Err(error) => runtime.hook_failed(error)?,
        }
    }
    let options = SimpleStreamOptions {
        signal: Some(runtime.signal()),
        headers: stream_options.headers.clone().unwrap_or_default(),
        timeout_ms: stream_options.timeout_ms,
        max_retries: stream_options.max_retries,
        max_retry_delay_ms: stream_options.max_retry_delay_ms,
        transport: stream_options.transport,
        cache_retention: stream_options.cache_retention,
        metadata: stream_options
            .metadata
            .as_ref()
            .map(|m| Json::from(m.clone()).to_serde())
            .and_then(|v| v.as_object().cloned()),
        deferred: stream_options.deferred.clone(),
        session_id: Some(ensure_provider_session_id(&runtime, &context).await?),
        reasoning: (thinking_level != pi_ai::ThinkingLevel::Off).then_some(thinking_level),
        ..SimpleStreamOptions::default()
    };
    let message = stream_response(&runtime, &model, messages, options, attempt, &context).await?;
    let request =
        Request { attempt, compacted, model: model_ref, cutoff, messages: Some(view.messages), poll_at: None };
    classify(&runtime, request, message, &context).await
}

pub(super) async fn poll(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let GenerationCheckpoint::Poll { attempt, compacted, model: model_ref, cutoff, handle, poll_at } = decode(&task)?
    else {
        return Err(DurableError::error("Generation checkpoint is not in phase poll"));
    };
    let Some(model) = runtime.models().get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(&runtime, Some(&model_ref), &context).await;
    };
    runtime.sleep(poll_at, &context).await?;
    let options = DeferredFetchOptions { signal: Some(runtime.signal()), ..DeferredFetchOptions::default() };
    let message = runtime.models().fetch_deferred(&model, &handle, options).await;
    let request = Request { attempt, compacted, model: model_ref, cutoff, messages: None, poll_at: Some(poll_at) };
    classify(&runtime, request, message, &context).await
}

/// Commit one partial into `pi.live.generation.message`.
async fn commit_partial(runtime: TaskRuntime, partial: AssistantMessage, attempt: u32, context: Context) {
    let conversation_id = runtime.conversation_id();
    let result = async {
        // Stored as a whole `Message`, with its role, as upstream stores the `AssistantMessage`.
        let message = encode(&pi_ai::Message::Assistant(partial))?;
        let change = move |tx: Tx, _| async move {
            let draft = tx.doc(&LIVE_DOC, conversation_id).await?;
            draft.edit(|live| {
                live.generation.get_or_insert_with(|| LiveGeneration::new(attempt)).message = Some(message)
            })?;
            Ok(None)
        };
        runtime.commit(change, &context).await
    }
    .await;
    // Failures after an abort mark or close are expected; the committed state stays consistent.
    if let Err(error) = result
        && !runtime.signal().aborted()
    {
        runtime.report(&error);
    }
}

/// Stream one request and return the terminal message. Partials commit at most every `partial_interval_ms`
/// with one commit in flight; the end waits for that commit, so no stale partial lands after the outcome.
async fn stream_response(
    runtime: &TaskRuntime,
    model: &Model,
    messages: Vec<Message>,
    options: SimpleStreamOptions,
    attempt: u32,
    context: &Context,
) -> DurableResult<AssistantMessage> {
    let interval = Duration::from_millis(runtime.settings().progress.partial_interval_ms);
    let request = pi_ai::Context { system_prompt: None, messages, tools: Vec::new() };
    let mut events = runtime.models().stream_simple(model, &request, options);
    let mut pending: Option<AssistantMessage> = None;
    let mut deadline: Option<Instant> = None;
    let mut in_flight: Option<JoinHandle<()>> = None;
    let mut last: Option<AssistantMessage> = None;
    loop {
        let timer = async {
            match deadline {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let flight = async {
            match in_flight.as_mut() {
                Some(handle) => {
                    let _ = handle.await;
                }
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            event = events.next() => {
                let Some(event) = event else { break };
                if let Some(message) = event.final_message() {
                    last = Some(message.clone());
                    continue;
                }
                let partial = match event {
                    AssistantMessageEvent::Start { partial }
                    | AssistantMessageEvent::TextStart { partial, .. }
                    | AssistantMessageEvent::TextDelta { partial, .. }
                    | AssistantMessageEvent::TextEnd { partial, .. }
                    | AssistantMessageEvent::ThinkingStart { partial, .. }
                    | AssistantMessageEvent::ThinkingDelta { partial, .. }
                    | AssistantMessageEvent::ThinkingEnd { partial, .. }
                    | AssistantMessageEvent::ToolcallStart { partial, .. }
                    | AssistantMessageEvent::ToolcallDelta { partial, .. }
                    | AssistantMessageEvent::ToolcallEnd { partial, .. } => partial,
                    _ => continue,
                };
                // A partial without content shows nothing; a deferred response never gets past it.
                if partial.content.is_empty() {
                    continue;
                }
                pending = Some(partial);
                if deadline.is_none() && in_flight.is_none() {
                    deadline = Some(Instant::now() + interval);
                }
            }
            () = timer => {
                deadline = None;
                if let Some(partial) = pending.take() {
                    in_flight = Some(tokio::spawn(commit_partial(runtime.clone(), partial, attempt, context.clone())));
                }
            }
            () = flight => {
                in_flight = None;
                if pending.is_some() {
                    deadline = Some(Instant::now() + interval);
                }
            }
        }
    }
    if let Some(handle) = in_flight {
        let _ = handle.await;
    }
    Ok(last.unwrap_or_else(|| {
        let mut message = AssistantMessage::empty(model);
        message.stop_reason = StopReason::Error;
        message.error_message = Some("Stream ended without a terminal event".into());
        message
    }))
}

fn stop_reason_text(reason: StopReason) -> String {
    encode(&reason).ok().and_then(|json| json.as_str().map(str::to_owned)).unwrap_or_default()
}

/// Classify a terminal provider message in one commit that also clears the partial.
pub(super) async fn classify(
    runtime: &TaskRuntime,
    request: Request,
    message: AssistantMessage,
    context: &Context,
) -> DurableResult<()> {
    // An abort mark or close: the abort invocation or the reopened run handles the committed state.
    runtime.signal().throw_if_aborted().map_err(DurableError::from)?;
    let conversation_id = runtime.conversation_id();
    let Request { attempt, compacted, model, cutoff, .. } = request.clone_parts();
    if message.stop_reason == StopReason::Deferred
        && let Some(handle) = message.deferred.clone()
    {
        let after = handle.poll_after_ms.map_or(DEFAULT_POLL_AFTER_MS, |ms| ms as f64);
        let poll_at = (runtime.now() + after).max(request.poll_at.map_or(f64::NEG_INFINITY, |at| at + 1.0));
        let change = move |tx: Tx, _| async move {
            let draft = tx.doc(&LIVE_DOC, conversation_id).await?;
            let generation =
                LiveGeneration { deferred: Some(DeferredPoll { poll_at }), ..LiveGeneration::new(attempt) };
            draft.edit(|live| live.generation = Some(generation))?;
            running(&GenerationCheckpoint::Poll { attempt, compacted, model, cutoff, handle, poll_at })
        };
        return runtime.commit(change, context).await;
    }
    for hooks in runtime.hooks::<GenerationHooks>(context).await? {
        let Some(hook) = &hooks.after_response else { continue };
        if let Err(error) = hook(message.clone(), runtime.clone(), context.clone()).await {
            runtime.hook_failed(error)?;
        }
    }
    let calls: Vec<_> = message.tool_calls().cloned().collect();
    if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
        return super::round::start_tool_round(runtime, &request, message, calls, context).await;
    }
    if matches!(message.stop_reason, StopReason::Stop | StopReason::Length | StopReason::ToolUse) {
        return answer(runtime, message, context).await;
    }
    let settings = runtime.settings();
    let overflow = message.stop_reason == StopReason::Error && is_context_overflow(&message, None);
    let owner = runtime.task_id();
    let policy = settings.compaction_for(&model);
    if overflow && compacted.is_none() && policy.enabled {
        let view = runtime.context(conversation_id, context, Some(cutoff)).await?;
        if select_cut(&view, policy.keep_recent_tokens).is_some() {
            let text = message.error_message.clone().unwrap_or_else(|| "Context overflow".into());
            let change = move |tx: Tx, _| async move {
                append_assistant(&tx, conversation_id, &message).await?;
                tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.generation = None)?;
                let input = CompactionInput { reason: CompactionReason::Overflow, instructions: None };
                let child = create_compaction(&tx, conversation_id, input, Some(owner)).await?;
                let checkpoint =
                    GenerationCheckpoint::Prepare { attempt, compacted: Some(child), overflow: Some(text) };
                waiting(&checkpoint, vec![child])
            };
            return runtime.commit(change, context).await;
        }
    }
    let policy = settings.retry;
    // An overflow is never retried: only a compaction can make the next request fit.
    let retry = message.stop_reason == StopReason::Error
        && !overflow
        && is_retryable_assistant_error(&message)
        && policy.enabled
        && attempt <= policy.max_retries;
    let until = if retry {
        runtime.now() + retry_delay_ms(policy.base_delay_ms, policy.max_agent_delay_ms, attempt) as f64
    } else {
        0.0
    };
    let change = move |tx: Tx, _| async move {
        append_assistant(&tx, conversation_id, &message).await?;
        if retry {
            let error = message.error_message.clone().unwrap_or_default();
            let generation =
                LiveGeneration { retry: Some(RetryState { at: until, error }), ..LiveGeneration::new(attempt) };
            tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| live.generation = Some(generation))?;
            return running(&GenerationCheckpoint::Retry { attempt, compacted, until });
        }
        let text = message.error_message.clone().unwrap_or_else(|| {
            format!("Model response ended with stop reason {}", stop_reason_text(message.stop_reason))
        });
        let settlement = unanswered("model_error", Some(&text));
        edit_live(&tx, conversation_id, |tx, live| end_run(tx, live, owner, settlement)).await?;
        Ok(super::failed(&text, "model_error"))
    };
    runtime.commit(change, context).await
}

impl Request {
    fn clone_parts(&self) -> Request {
        Request {
            attempt: self.attempt,
            compacted: self.compacted,
            model: self.model.clone(),
            cutoff: self.cutoff,
            messages: None,
            poll_at: self.poll_at,
        }
    }
}

/// A final answer; the final boundary places queued items. The first `on_yield` continuation hands the run to a
/// successor generation, unless the boundary selected a user item or a reset.
async fn answer(runtime: &TaskRuntime, message: AssistantMessage, context: &Context) -> DurableResult<()> {
    let mut continuation: Option<UserInput> = None;
    for hooks in runtime.hooks::<GenerationHooks>(context).await? {
        let Some(hook) = &hooks.on_yield else { continue };
        if continuation.is_some() {
            break;
        }
        match hook(message.clone(), runtime.clone(), context.clone()).await {
            Ok(next) => continuation = next,
            Err(error) => runtime.hook_failed(error)?,
        }
    }
    let (conversation_id, owner, core) = (runtime.conversation_id(), runtime.task_id(), runtime.scheduler.core.clone());
    let change = move |tx: Tx, _| async move {
        // Queue modes are read on the Session line, when the boundary is decided.
        let mut boundary = prepare_boundary(&tx, conversation_id, &core.settings()).await?;
        let live = tx.doc(&LIVE_DOC, conversation_id).await?;
        let entry = append_assistant(&tx, conversation_id, &message).await?;
        let mut result = pi_chord::JsonMap::new();
        result.insert("entryId", encode(&entry.id)?);
        let result = completed(Json::from(result));
        let placed = apply_boundary(&tx, &mut boundary, BoundaryAt::Final, core.now()).await?;
        if let Some(continuation) = continuation
            && placed.users.is_empty()
            && !placed.reset
        {
            append_user(&tx, conversation_id, continuation, core.now()).await?;
            let successor = create_generation(&tx, conversation_id).await?;
            live.edit(|live| {
                hand_over(live, owner, successor);
                live.generation = None;
            })?;
            return Ok(result);
        }
        let mut state = live.get()?;
        end_run(&tx, &mut state, owner, SubmissionSettlement::Done { answer: entry.id })?;
        live.set(&state)?;
        if !placed.users.is_empty() {
            super::start_run(&tx, conversation_id, &live, placed.users).await?;
        }
        Ok(result)
    };
    runtime.commit(change, context).await
}
