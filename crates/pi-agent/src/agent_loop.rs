//! The agent loop: stream a response, run its tool calls, and repeat until the
//! model stops, steering and follow-up queues are empty, or the run is cancelled.

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, ToolCall, ToolResultMessage,
};
use tokio_util::sync::CancellationToken;

use crate::hooks::{
    AgentLoopTurnUpdate, AgentTurnContext, AgentTurnDecision, FinishTurnFn, PrepareNextTurnFn, PrepareRequestContext,
    PrepareRequestFn,
};
use crate::tool_execution::{execute_tool_calls, fail_truncated_tool_calls};
use crate::transcript::{declare_tool_changes, is_transcript_context};
use crate::types::{
    AfterToolCall, AfterToolCallResult, AgentEvent, BeforeToolCall, BeforeToolCallResult, SharedTool, ToolExecutionMode,
};

pub type StreamFn = Arc<dyn Fn(&Model, &Context, StreamOptions) -> AssistantMessageEventStream + Send + Sync>;
pub type MessageSource = Arc<dyn Fn() -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
pub type ApiKeyFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;
pub type TransformContextFn =
    Arc<dyn Fn(Vec<Message>, CancellationToken) -> BoxFuture<'static, Vec<Message>> + Send + Sync>;
pub type BeforeToolCallFn = Arc<
    dyn for<'a> Fn(BeforeToolCall<'a>, CancellationToken) -> BoxFuture<'a, Option<BeforeToolCallResult>> + Send + Sync,
>;
pub type AfterToolCallFn = Arc<
    dyn for<'a> Fn(AfterToolCall<'a>, CancellationToken) -> BoxFuture<'a, Option<AfterToolCallResult>> + Send + Sync,
>;
/// Converts the transcript to provider messages before each request. The default
/// drops `Message::Custom` entries; hosts map their custom roles to user messages.
pub type ConvertToLlmFn = Arc<dyn Fn(&[Message]) -> Vec<Message> + Send + Sync>;
/// Tools that nested calls ([`crate::ToolContext::execute_tool`]) resolve against.
pub type CallableToolsFn = Arc<dyn Fn() -> Vec<SharedTool> + Send + Sync>;
/// Receives loop events in order; the loop awaits each before it goes on. For `MessageEnd`, a
/// returned message with the same role replaces the finalized one (upstream handlers replace it
/// in place before it is persisted).
pub type EventSink = Arc<dyn Fn(AgentEvent) -> BoxFuture<'static, Option<Message>> + Send + Sync>;

/// An [`EventSink`] over a synchronous callback that never replaces messages.
pub fn sync_sink(callback: impl Fn(AgentEvent) + Send + Sync + 'static) -> EventSink {
    Arc::new(move |event| {
        callback(event);
        Box::pin(async { None })
    })
}

/// Emit `MessageStart` and `MessageEnd` for `message` and return it, replaced when the
/// `MessageEnd` sink asked for a replacement with the same role.
pub(crate) async fn emit_message(emit: &EventSink, message: Message, start: bool) -> Message {
    if start {
        emit(AgentEvent::MessageStart { message: message.clone() }).await;
    }
    finish_message(emit, message).await
}

/// Emit `MessageEnd` and return the possibly replaced message.
pub(crate) async fn finish_message(emit: &EventSink, message: Message) -> Message {
    match emit(AgentEvent::MessageEnd { message: message.clone() }).await {
        Some(replacement) if replacement.role() == message.role() => replacement,
        _ => message,
    }
}

/// Conversation state the loop reads and appends to.
#[derive(Clone, Default)]
pub struct AgentContext {
    /// `None`: the transcript's system messages carry the prompt and tool declarations, and the
    /// loop declares tool changes (upstream behavior). `Some`: sent beside the messages instead.
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<SharedTool>,
}

#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    /// Requested reasoning effort; `None` means off.
    pub reasoning: Option<ThinkingLevel>,
    /// Defaults to `pi_ai::stream`.
    pub stream_fn: Option<StreamFn>,
    pub api_key: Option<String>,
    /// Resolved per request so expiring tokens refresh; falls back to `api_key`.
    pub get_api_key: Option<ApiKeyFn>,
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub session_id: Option<String>,
    pub tool_execution: ToolExecutionMode,
    /// Messages to inject after the current turn's tool calls.
    pub get_steering_messages: Option<MessageSource>,
    /// Messages to process once the agent would otherwise stop.
    pub get_follow_up_messages: Option<MessageSource>,
    pub before_tool_call: Option<BeforeToolCallFn>,
    pub after_tool_call: Option<AfterToolCallFn>,
    /// Rewrites the messages sent to the model, for example to compact old turns.
    pub transform_context: Option<TransformContextFn>,
    pub convert_to_llm: Option<ConvertToLlmFn>,
    /// Runs after each turn's tool results, before `turn_end`; can end or extend the run.
    pub finish_turn: Option<FinishTurnFn>,
    /// Runs before every provider request; can replace context, model, and thinking level.
    pub prepare_request: Option<PrepareRequestFn>,
    /// Runs before each turn after the first; can replace state and append messages.
    pub prepare_next_turn: Option<PrepareNextTurnFn>,
    /// Tools callable from other tools. Defaults to the context's tools.
    pub callable_tools: Option<CallableToolsFn>,
    /// Base request options (transport, thinking budgets, headers, payload observers, ...).
    /// The fields above, the cancellation token, and the reasoning level take precedence.
    pub stream_options: StreamOptions,
}

/// The default `convert_to_llm`: keep provider roles, drop custom ones.
pub fn default_convert_to_llm(messages: &[Message]) -> Vec<Message> {
    messages.iter().filter(|message| !matches!(message, Message::Custom(_))).cloned().collect()
}

impl AgentLoopConfig {
    pub fn new(model: Model) -> Self {
        Self {
            model,
            reasoning: None,
            stream_fn: None,
            api_key: None,
            get_api_key: None,
            max_tokens: None,
            temperature: None,
            session_id: None,
            tool_execution: ToolExecutionMode::default(),
            get_steering_messages: None,
            get_follow_up_messages: None,
            before_tool_call: None,
            after_tool_call: None,
            transform_context: None,
            convert_to_llm: None,
            finish_turn: None,
            prepare_request: None,
            prepare_next_turn: None,
            callable_tools: None,
            stream_options: StreamOptions::default(),
        }
    }
}

/// Add `prompts` to the context and run the loop. Returns the messages added during the run.
pub async fn run_agent_loop(
    prompts: Vec<Message>,
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
) -> Vec<Message> {
    let prompts = declare_tool_changes(context, prompts);
    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    let mut new_messages = Vec::with_capacity(prompts.len());
    for message in prompts {
        let message = emit_message(&emit, message, true).await;
        context.messages.push(message.clone());
        new_messages.push(message);
    }
    run_loop(context, &mut new_messages, config, &emit, &cancel).await;
    new_messages
}

/// Continue from the existing context, for example to retry after an error.
/// The last message must be a user message or a tool result.
pub async fn run_agent_loop_continue(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: EventSink,
    cancel: CancellationToken,
) -> anyhow::Result<Vec<Message>> {
    match context.messages.last() {
        None => anyhow::bail!("Cannot continue: no messages in context"),
        Some(Message::Assistant(_)) => anyhow::bail!("Cannot continue from message role: assistant"),
        Some(_) => {}
    }
    emit(AgentEvent::AgentStart).await;
    emit(AgentEvent::TurnStart).await;
    let mut new_messages = Vec::new();
    run_loop(context, &mut new_messages, config, &emit, &cancel).await;
    Ok(new_messages)
}

async fn take(source: &Option<MessageSource>) -> Vec<Message> {
    match source {
        Some(source) => source().await,
        None => Vec::new(),
    }
}

async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<Message>,
    initial_config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) {
    let mut config = initial_config.clone();
    let mut last_turn: Option<(AssistantMessage, Vec<ToolResultMessage>)> = None;
    let mut explicit_continuation = false;
    // The user may have typed while the run was starting.
    let mut pending = take(&config.get_steering_messages).await;
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            let mut prepared = Vec::new();
            if let Some((message, tool_results)) = &last_turn {
                if let Some(prepare) = config.prepare_next_turn.clone() {
                    let turn = AgentTurnContext { message, tool_results, context, new_messages };
                    if let Some(update) = prepare(turn, cancel.clone()).await {
                        prepared = apply_update(context, &mut config, update);
                    }
                }
                // Preparation can take long (compaction). Poll again only when the earlier poll
                // was empty, so one-at-a-time mode never delivers two messages in one turn.
                if pending.is_empty() {
                    pending = take(&config.get_steering_messages).await;
                }
                emit(AgentEvent::TurnStart).await;
            }
            prepared.append(&mut pending);
            for message in declare_tool_changes(context, prepared) {
                let message = emit_message(emit, message, true).await;
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            if let Some(prepare) = config.prepare_request.clone() {
                let thinking_level = config.reasoning.unwrap_or(ThinkingLevel::Off);
                let request = PrepareRequestContext { context, model: &config.model, thinking_level };
                if let Some(update) = prepare(request, cancel.clone()).await {
                    apply_update(context, &mut config, update.into());
                }
            }

            let message = stream_assistant_response(context, &config, emit, cancel).await;
            new_messages.push(Message::Assistant(message.clone()));
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                finish_turn(&config.finish_turn, &message, &[], context, new_messages, cancel).await;
                emit(AgentEvent::TurnEnd { message: Message::Assistant(message), tool_results: Vec::new() }).await;
                emit(AgentEvent::AgentEnd { messages: new_messages.clone() }).await;
                return;
            }

            let tool_calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_tool_calls(&tool_calls, emit).await
                } else {
                    execute_tool_calls(context, &message, &tool_calls, &config, emit, cancel).await
                };
                has_more_tool_calls = !batch.terminate;
                tool_results = batch.messages;
                for result in &tool_results {
                    context.messages.push(Message::ToolResult(result.clone()));
                    new_messages.push(Message::ToolResult(result.clone()));
                }
            }
            let decision =
                finish_turn(&config.finish_turn, &message, &tool_results, context, new_messages, cancel).await;
            emit(AgentEvent::TurnEnd {
                message: Message::Assistant(message.clone()),
                tool_results: tool_results.clone(),
            })
            .await;
            if decision == Some(AgentTurnDecision::End) || cancel.is_cancelled() {
                emit(AgentEvent::AgentEnd { messages: new_messages.clone() }).await;
                return;
            }
            explicit_continuation = decision == Some(AgentTurnDecision::Continue);
            pending = take(&config.get_steering_messages).await;
            if has_more_tool_calls || !pending.is_empty() {
                explicit_continuation = false;
            }
            last_turn = Some((message, tool_results));
        }

        let follow_ups = take(&config.get_follow_up_messages).await;
        if !follow_ups.is_empty() {
            explicit_continuation = false;
            pending = follow_ups;
            continue;
        }
        // No queued input selected a request, so honor `Continue` with one context-only turn.
        if explicit_continuation {
            explicit_continuation = false;
            continue;
        }
        break;
    }
    emit(AgentEvent::AgentEnd { messages: new_messages.clone() }).await;
}

async fn finish_turn(
    hook: &Option<FinishTurnFn>,
    message: &AssistantMessage,
    tool_results: &[ToolResultMessage],
    context: &AgentContext,
    new_messages: &[Message],
    cancel: &CancellationToken,
) -> Option<AgentTurnDecision> {
    let hook = hook.as_ref()?;
    hook(AgentTurnContext { message, tool_results, context, new_messages }, cancel.clone()).await
}

/// Apply replacement state from a hook and return the messages it asked to append.
fn apply_update(context: &mut AgentContext, config: &mut AgentLoopConfig, update: AgentLoopTurnUpdate) -> Vec<Message> {
    if let Some(next) = update.context {
        *context = next;
    }
    if let Some(model) = update.model {
        config.model = model;
    }
    if let Some(level) = update.thinking_level {
        config.reasoning = (level != ThinkingLevel::Off).then_some(level);
    }
    update.messages
}

async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) -> AssistantMessage {
    let mut messages = context.messages.clone();
    if let Some(transform) = &config.transform_context {
        messages = transform(messages, cancel.clone()).await;
    }
    let messages = match &config.convert_to_llm {
        Some(convert) => convert(&messages),
        None => default_convert_to_llm(&messages),
    };
    // A transcript context reaches the provider as messages only: system messages carry the
    // prompt and the tool declarations.
    let request = if is_transcript_context(context) {
        Context { system_prompt: None, messages, tools: Vec::new() }
    } else {
        Context {
            system_prompt: context.system_prompt.clone(),
            messages,
            tools: context.tools.iter().map(|tool| tool.declaration()).collect(),
        }
    };
    let api_key =
        config.get_api_key.as_ref().and_then(|get| get(&config.model.provider)).or_else(|| config.api_key.clone());
    let base = &config.stream_options;
    let options = StreamOptions {
        api_key: api_key.or_else(|| base.api_key.clone()),
        cancel: cancel.clone(),
        max_tokens: config.max_tokens.or(base.max_tokens),
        temperature: config.temperature.or(base.temperature),
        reasoning: config.reasoning.filter(|level| *level != ThinkingLevel::Off),
        session_id: config.session_id.clone().or_else(|| base.session_id.clone()),
        ..base.clone()
    };
    let mut response = match &config.stream_fn {
        Some(stream_fn) => stream_fn(&config.model, &request, options),
        None => pi_ai::stream(&config.model, &request, options),
    };

    let mut added_partial = false;
    while let Some(event) = response.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                context.messages.push(Message::Assistant(partial.clone()));
                added_partial = true;
                emit(AgentEvent::MessageStart { message: Message::Assistant(partial.clone()) }).await;
            }
            AssistantMessageEvent::Done { message, .. } | AssistantMessageEvent::Error { error: message, .. } => {
                let mut message = message.clone();
                // Record the requested level, whichever stream function answered.
                message.thinking_level = Some(config.reasoning.unwrap_or(ThinkingLevel::Off));
                return finish_response(context, message, added_partial, emit).await;
            }
            AssistantMessageEvent::TextStart { partial, .. }
            | AssistantMessageEvent::TextDelta { partial, .. }
            | AssistantMessageEvent::TextEnd { partial, .. }
            | AssistantMessageEvent::ThinkingStart { partial, .. }
            | AssistantMessageEvent::ThinkingDelta { partial, .. }
            | AssistantMessageEvent::ThinkingEnd { partial, .. }
            | AssistantMessageEvent::ToolcallStart { partial, .. }
            | AssistantMessageEvent::ToolcallDelta { partial, .. }
            | AssistantMessageEvent::ToolcallEnd { partial, .. } => {
                if added_partial {
                    let message = Message::Assistant(partial.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = message.clone();
                    }
                    emit(AgentEvent::MessageUpdate { message, assistant_message_event: event.clone() }).await;
                }
            }
        }
    }
    let mut message = AssistantMessage::empty(&config.model);
    message.stop_reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
    message.error_message = Some("Provider stream ended without a final event".into());
    message.thinking_level = Some(config.reasoning.unwrap_or(ThinkingLevel::Off));
    finish_response(context, message, added_partial, emit).await
}

async fn finish_response(
    context: &mut AgentContext,
    message: AssistantMessage,
    added_partial: bool,
    emit: &EventSink,
) -> AssistantMessage {
    let wrapped = Message::Assistant(message.clone());
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = wrapped.clone();
        }
    } else {
        context.messages.push(wrapped.clone());
        emit(AgentEvent::MessageStart { message: wrapped.clone() }).await;
    }
    let finished = finish_message(emit, wrapped).await;
    if let Some(last) = context.messages.last_mut() {
        *last = finished.clone();
    }
    match finished {
        Message::Assistant(replaced) => replaced,
        _ => message,
    }
}
