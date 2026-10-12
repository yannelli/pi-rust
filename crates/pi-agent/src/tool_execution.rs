//! Tool call preparation, execution, hooks, and result messages for one turn.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use futures::future::join_all;
use pi_ai::{AssistantMessage, Message, ToolCall, ToolResultMessage, now_ms};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AgentContext, AgentLoopConfig, EventSink};
use crate::types::{
    AfterToolCall, AgentEvent, AgentToolResult, BeforeToolCall, SharedTool, ToolExecutionMode, ToolUpdateCallback,
};
use crate::validate::validate_tool_arguments;

pub(crate) struct ToolBatch {
    pub messages: Vec<ToolResultMessage>,
    /// Every result asked to end the run.
    pub terminate: bool,
}

struct Finalized {
    call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
    duration_ms: Option<u64>,
}

enum Prepared {
    Immediate(Finalized),
    Ready { tool: SharedTool, call: ToolCall, args: Value },
}

/// A response cut off by the output limit may carry incomplete arguments, so none of its calls run.
pub(crate) fn fail_truncated_tool_calls(calls: &[ToolCall], emit: &EventSink) -> ToolBatch {
    let mut messages = Vec::new();
    for call in calls {
        emit_start(call, emit);
        let finalized = Finalized {
            call: call.clone(),
            result: AgentToolResult::text(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                call.name
            )),
            is_error: true,
            duration_ms: None,
        };
        messages.push(emit_end_and_result(&finalized, emit));
    }
    ToolBatch { messages, terminate: false }
}

pub(crate) async fn execute_tool_calls(
    context: &AgentContext,
    message: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) -> ToolBatch {
    let sequential = config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context
                .tools
                .iter()
                .any(|tool| tool.name() == call.name && tool.execution_mode() == Some(ToolExecutionMode::Sequential))
        });
    let mut finalized_calls = Vec::new();
    let mut messages = Vec::new();
    if sequential {
        for call in calls {
            emit_start(call, emit);
            let finalized = match prepare(context, message, call, config, cancel).await {
                Prepared::Immediate(finalized) => finalized,
                Prepared::Ready { tool, call, args } => {
                    run_prepared(tool, call, args, message, config, emit, cancel).await
                }
            };
            messages.push(emit_end_and_result(&finalized, emit));
            finalized_calls.push(finalized);
            if cancel.is_cancelled() {
                break;
            }
        }
    } else {
        let mut prepared = Vec::new();
        for call in calls {
            emit_start(call, emit);
            let preparation = prepare(context, message, call, config, cancel).await;
            if let Prepared::Immediate(finalized) = &preparation {
                emit_end(finalized, emit);
            }
            prepared.push(preparation);
            if cancel.is_cancelled() {
                break;
            }
        }
        let runs = prepared.into_iter().map(|preparation| async move {
            match preparation {
                Prepared::Immediate(finalized) => finalized,
                Prepared::Ready { tool, call, args } => {
                    let finalized = if cancel.is_cancelled() {
                        Finalized {
                            call,
                            result: AgentToolResult::text("Operation aborted"),
                            is_error: true,
                            duration_ms: None,
                        }
                    } else {
                        run_prepared(tool, call, args, message, config, emit, cancel).await
                    };
                    emit_end(&finalized, emit);
                    finalized
                }
            }
        });
        for finalized in join_all(runs).await {
            messages.push(emit_result(&finalized, emit));
            finalized_calls.push(finalized);
        }
    }
    let terminate = !finalized_calls.is_empty() && finalized_calls.iter().all(|finalized| finalized.result.terminate);
    ToolBatch { messages, terminate }
}

async fn prepare(
    context: &AgentContext,
    message: &AssistantMessage,
    call: &ToolCall,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> Prepared {
    let immediate = |text: String, terminate: bool| {
        Prepared::Immediate(Finalized {
            call: call.clone(),
            result: AgentToolResult { terminate, is_error: true, ..AgentToolResult::text(text) },
            is_error: true,
            duration_ms: None,
        })
    };
    let Some(tool) = context.tools.iter().find(|tool| tool.name() == call.name) else {
        return immediate(format!("Tool {} not found", call.name), false);
    };
    let args = match validate_tool_arguments(&call.name, &tool.parameters(), &Value::Object(call.arguments.clone())) {
        Ok(args) => args,
        Err(error) => return immediate(error, false),
    };
    if let Some(before) = &config.before_tool_call {
        let input = BeforeToolCall { assistant_message: message, tool_call: call, args: &args };
        let decision = before(input, cancel.clone()).await;
        if cancel.is_cancelled() {
            return immediate("Operation aborted".into(), false);
        }
        if let Some(decision) = decision.filter(|decision| decision.block) {
            let reason = decision.reason.unwrap_or_else(|| "Tool execution was blocked".into());
            return immediate(reason, decision.terminate);
        }
    }
    if cancel.is_cancelled() {
        return immediate("Operation aborted".into(), false);
    }
    Prepared::Ready { tool: tool.clone(), call: call.clone(), args }
}

async fn run_prepared(
    tool: SharedTool,
    call: ToolCall,
    args: Value,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) -> Finalized {
    let started = Instant::now();
    // Updates that arrive after the tool returns are dropped, as in the TypeScript loop.
    let accepting = Arc::new(AtomicBool::new(true));
    let on_update: ToolUpdateCallback = {
        let (emit, accepting) = (emit.clone(), accepting.clone());
        let (id, name, update_args) = (call.id.clone(), call.name.clone(), Value::Object(call.arguments.clone()));
        Arc::new(move |partial_result| {
            if accepting.load(Ordering::SeqCst) {
                emit(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    args: update_args.clone(),
                    partial_result,
                });
            }
        })
    };
    let outcome = tool.execute(&call.id, args.clone(), cancel.child_token(), on_update).await;
    accepting.store(false, Ordering::SeqCst);
    let duration_ms = Some(started.elapsed().as_millis() as u64);
    let (mut result, mut is_error) = match outcome {
        Ok(result) => {
            let is_error = result.is_error;
            (result, is_error)
        }
        Err(error) => (AgentToolResult::text(format!("{error:#}")), true),
    };
    if let Some(after) = &config.after_tool_call {
        let input =
            AfterToolCall { assistant_message: message, tool_call: &call, args: &args, result: &result, is_error };
        if let Some(update) = after(input, cancel.clone()).await {
            if let Some(content) = update.content {
                result.content = content;
            }
            if let Some(details) = update.details {
                result.details = Some(details);
            }
            if let Some(terminate) = update.terminate {
                result.terminate = terminate;
            }
            is_error = update.is_error.unwrap_or(is_error);
        }
    }
    result.is_error = is_error;
    Finalized { call, result, is_error, duration_ms }
}

fn emit_start(call: &ToolCall, emit: &EventSink) {
    emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: Value::Object(call.arguments.clone()),
    });
}

fn emit_end(finalized: &Finalized, emit: &EventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.call.id.clone(),
        tool_name: finalized.call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
        duration_ms: finalized.duration_ms,
    });
}

fn emit_result(finalized: &Finalized, emit: &EventSink) -> ToolResultMessage {
    let result = ToolResultMessage {
        tool_call_id: finalized.call.id.clone(),
        tool_name: finalized.call.name.clone(),
        content: finalized.result.content.clone(),
        details: finalized.result.details.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
        duration_ms: finalized.duration_ms,
    };
    emit(AgentEvent::MessageStart { message: Message::ToolResult(result.clone()) });
    emit(AgentEvent::MessageEnd { message: Message::ToolResult(result.clone()) });
    result
}

fn emit_end_and_result(finalized: &Finalized, emit: &EventSink) -> ToolResultMessage {
    emit_end(finalized, emit);
    emit_result(finalized, emit)
}
