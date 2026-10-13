//! Agent loop events and hooks for extensions (upstream `_emitExtensionEvent`, `_beforeToolCall`,
//! `_afterToolCall`, `transformContext`, the turn boundary hooks, and the provider request hooks
//! of `sdk.ts`).

use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use pi_agent::{
    AfterToolCall, AfterToolCallResult, AgentEvent, AgentEventHookFn, BeforeToolCall, BeforeToolCallResult,
};
use pi_ai::{AssistantMessage, Message, Model, StopReason, StreamOptions, ToolResultMessage};
use serde_json::{Value, json};

use super::{AgentSession, lock};
use crate::extensions::ExtensionRunner;

/// How long a `before_provider_request` handler may hold a request.
const PAYLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// Extension hooks around provider requests, shared with the session's stream function.
pub type RequestHooksSlot = Arc<RwLock<Option<Weak<ExtensionRunner>>>>;

fn slot_runner(slot: &RequestHooksSlot) -> Option<Arc<ExtensionRunner>> {
    slot.read().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().and_then(Weak::upgrade)
}

/// `before_provider_headers` after the attribution headers merged into `options`.
pub(crate) async fn apply_header_hooks(slot: &RequestHooksSlot, options: &mut StreamOptions) {
    let Some(runner) = slot_runner(slot).filter(|runner| runner.has_handlers("before_provider_headers")) else {
        return;
    };
    let headers = crate::extensions::convert::headers_to_json(&options.headers, &options.suppressed_headers);
    if let Ok(changed) = runner.call_json("emitBeforeProviderHeaders", vec![headers]).await {
        let (headers, suppressed) = crate::extensions::convert::headers_from_json(&changed);
        options.headers = headers;
        options.suppressed_headers = suppressed;
    }
}

/// Payload, response, and stream event observers for one request.
pub(crate) fn install_request_observers(slot: &RequestHooksSlot, options: &mut StreamOptions) {
    let Some(runner) = slot_runner(slot) else { return };
    if runner.has_handlers("before_provider_request") {
        let weak = Arc::downgrade(&runner);
        options.on_payload = Some(Arc::new(move |payload: &Value, _model: &Model| {
            let runner = weak.upgrade()?;
            let call = vec![payload.clone().into()];
            let result = runner.host().blocking_call_method_within("emitBeforeProviderRequest", call, PAYLOAD_TIMEOUT);
            result.ok().map(pi_js::HostValue::into_json).filter(|changed| changed != payload)
        }));
    }
    if runner.has_handlers("after_provider_response") {
        let weak = Arc::downgrade(&runner);
        options.on_response = Some(Arc::new(move |response: &pi_ai::ProviderResponse, _model: &Model| {
            if let Some(runner) = weak.upgrade() {
                let event =
                    json!({"type": "after_provider_response", "status": response.status, "headers": response.headers});
                runner.emit_detached(event);
            }
        }));
    }
    if runner.has_handlers("provider_stream_event") {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let forward = runner.clone();
        tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                forward.emit(event).await;
            }
        });
        options.on_provider_stream_event = Some(Arc::new(move |data: &Value, model: &Model| {
            let event = json!({
                "type": "provider_stream_event",
                "data": data,
                "provider": model.provider,
                "api": model.api,
                "model": model.id,
            });
            let _ = sender.send(event);
        }));
    }
}

fn outcome(message: &AssistantMessage) -> &'static str {
    match message.stop_reason {
        StopReason::Aborted => "aborted",
        StopReason::Error => "error",
        _ => "completed",
    }
}

impl AgentSession {
    pub(crate) fn install_request_hooks(&self, runner: &Arc<ExtensionRunner>) {
        *self.request_hooks.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::downgrade(runner));
    }

    /// Loop events in upstream extension form; `message_end` handlers may replace the message.
    async fn emit_agent_event(&self, event: AgentEvent) -> Option<Message> {
        let runner = self.extension_runner()?;
        let state = &self.extensions;
        let event = match event {
            AgentEvent::AgentStart => {
                state.turn_index.store(0, Ordering::SeqCst);
                json!({"type": "agent_start"})
            }
            AgentEvent::AgentEnd { messages } => json!({"type": "agent_end", "messages": messages}),
            AgentEvent::TurnStart => json!({
                "type": "turn_start",
                "turnIndex": state.turn_index.load(Ordering::SeqCst),
                "timestamp": pi_ai::now_ms(),
            }),
            AgentEvent::TurnEnd { message, tool_results } => {
                if let Message::Assistant(assistant) = &message {
                    let dispatched = {
                        let mut dispatched = lock(&state.dispatched);
                        let index = dispatched.iter().position(|stamp| *stamp == assistant.timestamp);
                        index.map(|index| dispatched.remove(index)).is_some()
                    };
                    if !dispatched {
                        self.dispatch_turn_end(&runner, assistant, &tool_results).await;
                    }
                }
                state.turn_index.fetch_add(1, Ordering::SeqCst);
                return None;
            }
            AgentEvent::MessageStart { message } => json!({"type": "message_start", "message": message}),
            AgentEvent::MessageUpdate { message, assistant_message_event } => json!({
                "type": "message_update",
                "message": message,
                "assistantMessageEvent": assistant_message_event,
            }),
            AgentEvent::MessageEnd { message } => {
                if !runner.has_handlers("message_end") {
                    return None;
                }
                let event = json!({"type": "message_end", "message": message});
                let replaced = runner.call_json("emitMessageEnd", vec![event]).await.ok()?;
                return crate::extensions::convert::message_from_json(replaced);
            }
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, args, .. } => json!({
                "type": "tool_execution_start",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "args": args,
            }),
            AgentEvent::ToolExecutionUpdate { tool_call_id, tool_name, args, partial_result, .. } => json!({
                "type": "tool_execution_update",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "args": args,
                "partialResult": partial_result,
            }),
            AgentEvent::ToolExecutionEnd { tool_call_id, tool_name, result, is_error, duration_ms, .. } => {
                let mut event = json!({
                    "type": "tool_execution_end",
                    "toolCallId": tool_call_id,
                    "toolName": tool_name,
                    "result": result,
                    "isError": is_error,
                });
                if let Some(duration) = duration_ms {
                    event["durationMs"] = json!(duration);
                }
                event
            }
        };
        runner.emit(event).await;
        None
    }

    /// `tool_call` handlers: a throwing handler blocks the call; edits of `input` apply.
    pub(crate) async fn extension_tool_call(&self, call: &BeforeToolCall<'_>) -> Option<BeforeToolCallResult> {
        let runner = self.extension_runner().filter(|runner| runner.has_handlers("tool_call"))?;
        let mut event = json!({
            "type": "tool_call",
            "toolName": call.tool_call.name,
            "toolCallId": call.tool_call.id,
            "input": call.args,
        });
        if let Some(parent) = call.parent_tool_call_id {
            event["parentToolCallId"] = json!(parent);
        }
        match runner.call_json("emitToolCall", vec![event]).await {
            Ok(result) => {
                let input = result.get("input").filter(|input| *input != call.args).cloned();
                let block = result["block"].as_bool().unwrap_or(false);
                let reason = result["reason"].as_str().map(str::to_string);
                (block || input.is_some()).then_some(BeforeToolCallResult {
                    block,
                    reason,
                    terminate: false,
                    args: input,
                })
            }
            Err(error) => Some(BeforeToolCallResult { block: true, reason: Some(error.message), ..Default::default() }),
        }
    }

    /// `tool_result` handlers: changed content, details, and error state.
    pub(crate) async fn extension_tool_result(&self, call: &AfterToolCall<'_>) -> Option<AfterToolCallResult> {
        let runner = self.extension_runner().filter(|runner| runner.has_handlers("tool_result"))?;
        let mut event = json!({
            "type": "tool_result",
            "toolName": call.tool_call.name,
            "toolCallId": call.tool_call.id,
            "input": call.args,
            "content": call.result.content,
            "details": call.result.details,
            "isError": call.is_error,
            "usage": call.result.usage,
        });
        if let Some(structured) = &call.result.structured_content {
            event["structuredContent"] = structured.clone();
        }
        if let Some(parent) = call.parent_tool_call_id {
            event["parentToolCallId"] = json!(parent);
        }
        let result = runner.call_json("emitToolResult", vec![event]).await.ok().filter(|result| !result.is_null())?;
        Some(AfterToolCallResult {
            content: serde_json::from_value(result["content"].clone()).ok(),
            details: result.get("details").cloned(),
            structured_content: result.get("structuredContent").filter(|value| !value.is_null()).cloned(),
            is_error: result["isError"].as_bool(),
            usage: serde_json::from_value(result["usage"].clone()).ok(),
            terminate: None,
        })
    }

    /// `context` and `context_with_system` handlers on the request messages.
    pub(crate) async fn extension_context(&self, messages: Vec<Message>) -> Vec<Message> {
        let Some(runner) = self
            .extension_runner()
            .filter(|runner| runner.has_handlers("context") || runner.has_handlers("context_with_system"))
        else {
            return messages;
        };
        match runner.call_json("emitContext", vec![json!(messages)]).await {
            Ok(changed) => serde_json::from_value(changed).unwrap_or(messages),
            Err(_) => messages,
        }
    }

    /// `turn_end` from the loop's `finish_turn`: whether a handler asked to continue.
    pub(crate) async fn finish_turn_boundary(&self, message: &AssistantMessage, results: &[ToolResultMessage]) -> bool {
        let Some(runner) = self.extension_runner() else { return false };
        lock(&self.extensions.dispatched).push(message.timestamp);
        self.dispatch_turn_end(&runner, message, results).await
    }

    async fn dispatch_turn_end(
        &self,
        runner: &ExtensionRunner,
        message: &AssistantMessage,
        results: &[ToolResultMessage],
    ) -> bool {
        let outcome = outcome(message);
        *lock(&self.extensions.outcome) = outcome;
        if !runner.has_handlers("turn_end") {
            return false;
        }
        let (entry_id, result_ids) = {
            let state = lock(&self.state);
            let entry = state.last_assistant.as_ref().filter(|(last, _)| last.timestamp == message.timestamp);
            (entry.and_then(|(_, id)| id.clone()), state.turn_tool_results.clone())
        };
        let Some(entry_id) = entry_id else {
            runner.emit_error(crate::extensions::ExtensionError {
                extension_path: "<boundary>".into(),
                event: "turn_end".into(),
                error: "turn_end could not resolve the persisted assistant entry ID".into(),
                stack: None,
            });
            return false;
        };
        let event = json!({
            "type": "turn_end",
            "turnIndex": self.extensions.turn_index.load(Ordering::SeqCst),
            "message": Message::Assistant(message.clone()),
            "toolResults": results.iter().cloned().map(Message::ToolResult).collect::<Vec<_>>(),
            "messageEntryId": entry_id,
            "toolResultEntryIds": result_ids,
            "outcome": outcome,
        });
        self.run_boundary(runner, event, "turn_end").await
    }

    /// `agent_before_settle`: whether the run continues before it settles.
    pub(crate) async fn before_settle_boundary(&self) -> bool {
        let runner = self.extension_runner().filter(|runner| runner.has_handlers("agent_before_settle"));
        let Some(runner) = runner else { return self.agent.has_queued_messages() };
        let outcome = *lock(&self.extensions.outcome);
        let event = json!({"type": "agent_before_settle", "outcome": outcome});
        let continued = self.run_boundary(&runner, event, "agent_before_settle").await;
        self.flush_pending_custom_messages();
        let should_continue = continued || self.agent.has_queued_messages();
        if should_continue && !self.boundary_can_continue("agent_before_settle") {
            if continued {
                report_invalid_continuation(&runner, "agent_before_settle");
            }
            return false;
        }
        should_continue
    }

    async fn run_boundary(&self, runner: &ExtensionRunner, event: Value, kind: &str) -> bool {
        let Ok(result) = runner.call_json("emitBoundary", vec![event]).await else { return false };
        let drafts = result["entries"].as_array().cloned().unwrap_or_default();
        self.commit_boundary_drafts(&drafts);
        let continued = result["continue"].as_bool().unwrap_or(false);
        if continued && kind == "turn_end" && !self.boundary_can_continue(kind) {
            report_invalid_continuation(runner, kind);
            return false;
        }
        continued
    }
}

fn report_invalid_continuation(runner: &ExtensionRunner, event: &str) {
    runner.emit_error(crate::extensions::ExtensionError {
        extension_path: "<boundary>".into(),
        event: event.into(),
        error: format!("{event} requested continuation without runnable model context"),
        stack: None,
    });
}

/// `on_event` for the agent: extension events before session listeners, as upstream awaits them.
pub(crate) fn agent_event_hook(weak: Weak<AgentSession>) -> AgentEventHookFn {
    Arc::new(move |event, _cancel| -> BoxFuture<'static, Option<Message>> {
        let weak = weak.clone();
        async move {
            let session = weak.upgrade()?;
            session.emit_agent_event(event).await
        }
        .boxed()
    })
}
