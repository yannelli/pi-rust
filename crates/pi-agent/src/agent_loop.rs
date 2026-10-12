//! The agent loop: stream a response, run its tool calls, and repeat until the
//! model stops, steering and follow-up queues are empty, or the run is cancelled.

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use pi_ai::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, Message, Model, StopReason,
    StreamOptions, ThinkingLevel, ToolCall,
};
use tokio_util::sync::CancellationToken;

use crate::tool_execution::{execute_tool_calls, fail_truncated_tool_calls};
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
/// Receives loop events synchronously, in order.
pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Conversation state the loop reads and appends to.
#[derive(Clone, Default)]
pub struct AgentContext {
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
    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
    for message in &prompts {
        emit(AgentEvent::MessageStart { message: message.clone() });
        emit(AgentEvent::MessageEnd { message: message.clone() });
    }
    context.messages.extend(prompts.iter().cloned());
    let mut new_messages = prompts;
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
    emit(AgentEvent::AgentStart);
    emit(AgentEvent::TurnStart);
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
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) {
    let mut first_turn = true;
    let mut pending = take(&config.get_steering_messages).await;
    loop {
        let mut has_more_tool_calls = true;
        while has_more_tool_calls || !pending.is_empty() {
            if !first_turn {
                emit(AgentEvent::TurnStart);
            }
            first_turn = false;
            for message in pending.drain(..) {
                emit(AgentEvent::MessageStart { message: message.clone() });
                emit(AgentEvent::MessageEnd { message: message.clone() });
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            let message = stream_assistant_response(context, config, emit, cancel).await;
            new_messages.push(Message::Assistant(message.clone()));
            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                emit(AgentEvent::TurnEnd { message: Message::Assistant(message), tool_results: Vec::new() });
                emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
                return;
            }

            let tool_calls: Vec<ToolCall> = message.tool_calls().cloned().collect();
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_truncated_tool_calls(&tool_calls, emit)
                } else {
                    execute_tool_calls(context, &message, &tool_calls, config, emit, cancel).await
                };
                has_more_tool_calls = !batch.terminate;
                tool_results = batch.messages;
                for result in &tool_results {
                    context.messages.push(Message::ToolResult(result.clone()));
                    new_messages.push(Message::ToolResult(result.clone()));
                }
            }
            emit(AgentEvent::TurnEnd { message: Message::Assistant(message), tool_results });
            if cancel.is_cancelled() {
                emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
                return;
            }
            pending = take(&config.get_steering_messages).await;
        }

        let follow_ups = take(&config.get_follow_up_messages).await;
        if follow_ups.is_empty() {
            break;
        }
        pending = follow_ups;
    }
    emit(AgentEvent::AgentEnd { messages: new_messages.clone() });
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
    let request = Context {
        system_prompt: context.system_prompt.clone(),
        messages,
        tools: context.tools.iter().map(|tool| tool.declaration()).collect(),
    };
    let api_key =
        config.get_api_key.as_ref().and_then(|get| get(&config.model.provider)).or_else(|| config.api_key.clone());
    let options = StreamOptions {
        api_key,
        cancel: cancel.clone(),
        max_tokens: config.max_tokens,
        temperature: config.temperature,
        reasoning: config.reasoning.filter(|level| *level != ThinkingLevel::Off),
        headers: Default::default(),
        session_id: config.session_id.clone(),
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
                emit(AgentEvent::MessageStart { message: Message::Assistant(partial.clone()) });
            }
            AssistantMessageEvent::Done { message, .. } | AssistantMessageEvent::Error { error: message, .. } => {
                return finish_response(context, message.clone(), added_partial, emit);
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
                    emit(AgentEvent::MessageUpdate { message, assistant_message_event: event.clone() });
                }
            }
        }
    }
    let mut message = AssistantMessage::empty(&config.model);
    message.stop_reason = if cancel.is_cancelled() { StopReason::Aborted } else { StopReason::Error };
    message.error_message = Some("Provider stream ended without a final event".into());
    finish_response(context, message, added_partial, emit)
}

fn finish_response(
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
        emit(AgentEvent::MessageStart { message: wrapped.clone() });
    }
    emit(AgentEvent::MessageEnd { message: wrapped });
    message
}
