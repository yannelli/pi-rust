//! Tool call preparation, execution, hooks, and result messages for one turn.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use futures::future::join_all;
use pi_ai::{AssistantMessage, Message, Model, ToolCall, ToolResultMessage, now_ms};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{AfterToolCallFn, AgentContext, AgentLoopConfig, BeforeToolCallFn, EventSink};
use crate::nested::{NestedToolCallHost, NestedToolCallRunner, ToolContext, combine_usage};
use crate::types::{
    AfterToolCall, AgentEvent, AgentToolCallOutcome, AgentToolResult, BeforeToolCall, BeforeToolCallResult, SharedTool,
    ToolExecutionMode, ToolUpdateCallback,
};
use crate::validate::validate_tool_arguments;

pub(crate) struct ToolBatch {
    pub messages: Vec<ToolResultMessage>,
    /// Every result asked to end the run.
    pub terminate: bool,
}

/// Preparations live for one batch, so the outcome stays inline.
#[allow(clippy::large_enum_variant)]
enum Prepared {
    Immediate(AgentToolCallOutcome),
    Ready { tool: SharedTool, call: ToolCall, args: Value },
}

/// Options for [`run_tool_call`].
#[derive(Clone)]
pub struct RunToolCallOptions<'a> {
    /// Tools the call resolves against.
    pub tools: &'a [SharedTool],
    /// Passed to the hooks as the message that issued the call.
    pub assistant_message: &'a AssistantMessage,
    /// Passed to the hooks as the current agent context.
    pub context: &'a AgentContext,
    pub before_tool_call: Option<&'a BeforeToolCallFn>,
    pub after_tool_call: Option<&'a AfterToolCallFn>,
    /// Reported to the hooks for calls made by another tool.
    pub parent_tool_call_id: Option<&'a str>,
    /// Context handed to the tool; defaults to one without nested calls.
    pub tool_context: Option<ToolContext>,
    pub cancel: CancellationToken,
    pub on_update: Option<ToolUpdateCallback>,
}

fn error_outcome(call: &ToolCall, text: impl Into<String>, terminate: bool) -> AgentToolCallOutcome {
    AgentToolCallOutcome {
        tool_call: call.clone(),
        result: AgentToolResult { terminate, ..AgentToolResult::error(text) },
        is_error: true,
        duration_ms: None,
    }
}

/// Run one call through the same steps as a model-issued call: argument preparation, schema
/// validation, `before_tool_call`, execution, and `after_tool_call`. Emits no events and adds no
/// messages. Unknown tools, validation errors, blocked calls, and failures come back as errors.
pub async fn run_tool_call(call: ToolCall, options: RunToolCallOptions<'_>) -> AgentToolCallOutcome {
    let RunToolCallOptions { tools, assistant_message, context, parent_tool_call_id, cancel, .. } = options;
    let before = options.before_tool_call;
    match prepare(tools, context, assistant_message, &call, before, parent_tool_call_id, &cancel).await {
        Prepared::Immediate(outcome) => outcome,
        Prepared::Ready { tool, call, args } => {
            let tool_context = options.tool_context.unwrap_or_else(|| ToolContext::new(call.id.clone()));
            let on_update = options.on_update.unwrap_or_else(|| Arc::new(|_| {}));
            let executed = execute_prepared(&tool, &args, tool_context, &cancel, on_update).await;
            let after = options.after_tool_call;
            finalize(context, assistant_message, call, &args, executed, after, parent_tool_call_id, &cancel).await
        }
    }
}

/// A response cut off by the output limit may carry incomplete arguments, so none of its calls run.
pub(crate) async fn fail_truncated_tool_calls(calls: &[ToolCall], emit: &EventSink) -> ToolBatch {
    let mut messages = Vec::new();
    for call in calls {
        emit_start(call, emit).await;
        let text = format!(
            "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
            call.name
        );
        let outcome = error_outcome(call, text, false);
        emit_end(&outcome, emit).await;
        messages.push(emit_result(&outcome, None, emit).await);
    }
    ToolBatch { messages, terminate: false }
}

/// Runs calls tools make while the batch executes, against the loop's tools and hooks.
struct LoopNestedHost {
    context: Arc<AgentContext>,
    message: AssistantMessage,
    config: AgentLoopConfig,
    emit: EventSink,
    model: Arc<Model>,
}

#[async_trait]
impl NestedToolCallHost for LoopNestedHost {
    fn tools(&self) -> Vec<SharedTool> {
        match &self.config.callable_tools {
            Some(callable) => callable(),
            None => self.context.tools.clone(),
        }
    }

    fn is_sequential(&self) -> bool {
        self.config.tool_execution == ToolExecutionMode::Sequential
    }

    async fn run_tool_call(
        &self,
        call: ToolCall,
        parent_tool_call_id: &str,
        context: ToolContext,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> AgentToolCallOutcome {
        let tools = self.tools();
        let options = RunToolCallOptions {
            tools: &tools,
            assistant_message: &self.message,
            context: &self.context,
            before_tool_call: self.config.before_tool_call.as_ref(),
            after_tool_call: self.config.after_tool_call.as_ref(),
            parent_tool_call_id: Some(parent_tool_call_id),
            tool_context: Some(context),
            cancel,
            on_update: Some(on_update),
        };
        run_tool_call(call, options).await
    }

    async fn emit(&self, event: AgentEvent) {
        (self.emit)(event).await;
    }

    fn model(&self) -> Option<Arc<Model>> {
        Some(self.model.clone())
    }
}

pub(crate) async fn execute_tool_calls(
    context: &mut AgentContext,
    message: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
) -> ToolBatch {
    // Shared with nested calls for the batch, then handed back without a copy.
    let shared = Arc::new(std::mem::take(context));
    let host = LoopNestedHost {
        context: shared.clone(),
        message: message.clone(),
        config: config.clone(),
        emit: emit.clone(),
        model: Arc::new(config.model.clone()),
    };
    let runner = NestedToolCallRunner::new(Arc::new(host));
    let batch = run_batch(&shared, message, calls, config, emit, cancel, &runner).await;
    drop(runner);
    *context = Arc::try_unwrap(shared).unwrap_or_else(|shared| (*shared).clone());
    batch
}

async fn run_batch(
    context: &AgentContext,
    message: &AssistantMessage,
    calls: &[ToolCall],
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
    runner: &Arc<NestedToolCallRunner>,
) -> ToolBatch {
    let sequential = config.tool_execution == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            context
                .tools
                .iter()
                .any(|tool| tool.name() == call.name && tool.execution_mode() == Some(ToolExecutionMode::Sequential))
        });
    let before = config.before_tool_call.as_ref();
    let mut outcomes = Vec::new();
    let mut messages = Vec::new();
    if sequential {
        for call in calls {
            emit_start(call, emit).await;
            let outcome = match prepare(&context.tools, context, message, call, before, None, cancel).await {
                Prepared::Immediate(outcome) => outcome,
                Prepared::Ready { tool, call, args } => {
                    run_prepared(context, tool, call, args, message, config, emit, cancel, runner).await
                }
            };
            emit_end(&outcome, emit).await;
            messages.push(emit_result(&outcome, Some(runner), emit).await);
            outcomes.push(outcome);
            if cancel.is_cancelled() {
                break;
            }
        }
    } else {
        let mut prepared = Vec::new();
        for call in calls {
            emit_start(call, emit).await;
            let preparation = prepare(&context.tools, context, message, call, before, None, cancel).await;
            if let Prepared::Immediate(outcome) = &preparation {
                emit_end(outcome, emit).await;
            }
            prepared.push(preparation);
            if cancel.is_cancelled() {
                break;
            }
        }
        let runs = prepared.into_iter().map(|preparation| async move {
            match preparation {
                Prepared::Immediate(outcome) => outcome,
                Prepared::Ready { tool, call, args } => {
                    let outcome = if cancel.is_cancelled() {
                        error_outcome(&call, "Operation aborted", false)
                    } else {
                        run_prepared(context, tool, call, args, message, config, emit, cancel, runner).await
                    };
                    emit_end(&outcome, emit).await;
                    outcome
                }
            }
        });
        for outcome in join_all(runs).await {
            messages.push(emit_result(&outcome, Some(runner), emit).await);
            outcomes.push(outcome);
        }
    }
    let terminate = !outcomes.is_empty() && outcomes.iter().all(|outcome| outcome.result.terminate);
    ToolBatch { messages, terminate }
}

async fn prepare(
    tools: &[SharedTool],
    context: &AgentContext,
    message: &AssistantMessage,
    call: &ToolCall,
    before: Option<&BeforeToolCallFn>,
    parent_tool_call_id: Option<&str>,
    cancel: &CancellationToken,
) -> Prepared {
    let immediate = |text: String, terminate: bool| Prepared::Immediate(error_outcome(call, text, terminate));
    let Some(tool) = tools.iter().find(|tool| tool.name() == call.name) else {
        return immediate(format!("Tool {} not found", call.name), false);
    };
    let prepared = match tool.prepare_arguments(Value::Object(call.arguments.clone())) {
        Ok(prepared) => prepared,
        Err(error) => return immediate(format!("{error:#}"), false),
    };
    let mut args = match validate_tool_arguments(&call.name, &tool.parameters(), &prepared) {
        Ok(args) => args,
        Err(error) => return immediate(error, false),
    };
    if let Some(before) = before {
        let input =
            BeforeToolCall { assistant_message: message, tool_call: call, args: &args, context, parent_tool_call_id };
        let decision = before(input, cancel.clone()).await;
        if cancel.is_cancelled() {
            return immediate("Operation aborted".into(), false);
        }
        match decision {
            Some(decision) if decision.block => {
                let reason = decision.reason.unwrap_or_else(|| "Tool execution was blocked".into());
                return immediate(reason, decision.terminate);
            }
            Some(BeforeToolCallResult { args: Some(replaced), .. }) => args = replaced,
            _ => {}
        }
    }
    if cancel.is_cancelled() {
        return immediate("Operation aborted".into(), false);
    }
    Prepared::Ready { tool: tool.clone(), call: call.clone(), args }
}

struct Executed {
    result: AgentToolResult,
    is_error: bool,
    duration_ms: u64,
}

async fn execute_prepared(
    tool: &SharedTool,
    args: &Value,
    tool_context: ToolContext,
    cancel: &CancellationToken,
    on_update: ToolUpdateCallback,
) -> Executed {
    let started = Instant::now();
    // Updates that arrive after the tool returns are dropped, as in the TypeScript loop.
    let accepting = Arc::new(AtomicBool::new(true));
    let guarded: ToolUpdateCallback = {
        let accepting = accepting.clone();
        Arc::new(move |partial| {
            if accepting.load(Ordering::SeqCst) {
                on_update(partial);
            }
        })
    };
    let outcome = tool.execute_with_context(&tool_context, args.clone(), cancel.child_token(), guarded).await;
    accepting.store(false, Ordering::SeqCst);
    let duration_ms = started.elapsed().as_millis() as u64;
    match outcome {
        Ok(result) => Executed { is_error: result.is_error, result, duration_ms },
        Err(error) => Executed { result: AgentToolResult::error(format!("{error:#}")), is_error: true, duration_ms },
    }
}

#[allow(clippy::too_many_arguments)]
async fn finalize(
    context: &AgentContext,
    message: &AssistantMessage,
    call: ToolCall,
    args: &Value,
    executed: Executed,
    after: Option<&AfterToolCallFn>,
    parent_tool_call_id: Option<&str>,
    cancel: &CancellationToken,
) -> AgentToolCallOutcome {
    let Executed { mut result, mut is_error, duration_ms } = executed;
    if let Some(after) = after {
        let input = AfterToolCall {
            assistant_message: message,
            tool_call: &call,
            args,
            result: &result,
            is_error,
            context,
            parent_tool_call_id,
        };
        if let Some(update) = after(input, cancel.clone()).await {
            // Structured content not replaced along with the content may no longer match it.
            if update.structured_content.is_some() || update.content.is_some() {
                result.structured_content = update.structured_content;
            }
            if let Some(content) = update.content {
                result.content = content;
            }
            if let Some(details) = update.details {
                result.details = Some(details);
            }
            if let Some(usage) = update.usage {
                result.usage = Some(usage);
            }
            if let Some(terminate) = update.terminate {
                result.terminate = terminate;
            }
            is_error = update.is_error.unwrap_or(is_error);
        }
    }
    result.is_error = is_error;
    AgentToolCallOutcome { tool_call: call, result, is_error, duration_ms: Some(duration_ms) }
}

#[allow(clippy::too_many_arguments)]
async fn run_prepared(
    context: &AgentContext,
    tool: SharedTool,
    call: ToolCall,
    args: Value,
    message: &AssistantMessage,
    config: &AgentLoopConfig,
    emit: &EventSink,
    cancel: &CancellationToken,
    runner: &Arc<NestedToolCallRunner>,
) -> AgentToolCallOutcome {
    let (id, name, update_args) = (call.id.clone(), call.name.clone(), Value::Object(call.arguments.clone()));
    let (on_update, updates) = update_relay(emit.clone(), move |partial_result| AgentEvent::ToolExecutionUpdate {
        tool_call_id: id.clone(),
        tool_name: name.clone(),
        args: update_args.clone(),
        partial_result,
        parent_tool_call_id: None,
    });
    let tool_context = ToolContext::nested(call.id.clone(), runner.clone(), cancel.clone());
    let executed = updates.run(execute_prepared(&tool, &args, tool_context, cancel, on_update)).await;
    finalize(context, message, call, &args, executed, config.after_tool_call.as_ref(), None, cancel).await
}

/// Delivers `tool_execution_update` events in order while a tool runs. The tool's update
/// callback is synchronous, so events queue until the relay emits them.
pub(crate) struct UpdateRelay {
    sender: futures::channel::mpsc::UnboundedSender<AgentEvent>,
    receiver: futures::channel::mpsc::UnboundedReceiver<AgentEvent>,
    emit: EventSink,
}

impl UpdateRelay {
    /// Run `execution` while emitting queued updates; updates sent after it ends are dropped.
    pub async fn run<T>(self, execution: impl std::future::Future<Output = T>) -> T {
        use futures::StreamExt;
        let Self { sender, mut receiver, emit } = self;
        let execution = async move {
            let output = execution.await;
            sender.close_channel();
            output
        };
        let drain = async move {
            while let Some(event) = receiver.next().await {
                emit(event).await;
            }
        };
        futures::join!(execution, drain).0
    }
}

/// An update callback that queues `make(partial)` events, and the relay that emits them.
pub(crate) fn update_relay(
    emit: EventSink,
    make: impl Fn(AgentToolResult) -> AgentEvent + Send + Sync + 'static,
) -> (ToolUpdateCallback, UpdateRelay) {
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    let queue = sender.clone();
    let callback: ToolUpdateCallback = Arc::new(move |partial| {
        let _ = queue.unbounded_send(make(partial));
    });
    (callback, UpdateRelay { sender, receiver, emit })
}

async fn emit_start(call: &ToolCall, emit: &EventSink) {
    emit(AgentEvent::ToolExecutionStart {
        tool_call_id: call.id.clone(),
        tool_name: call.name.clone(),
        args: Value::Object(call.arguments.clone()),
        parent_tool_call_id: None,
    })
    .await;
}

async fn emit_end(outcome: &AgentToolCallOutcome, emit: &EventSink) {
    emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        result: outcome.result.clone(),
        is_error: outcome.is_error,
        duration_ms: outcome.duration_ms,
        parent_tool_call_id: None,
    })
    .await;
}

/// Build the result message, with the record and usage of the call's nested calls.
async fn emit_result(
    outcome: &AgentToolCallOutcome,
    runner: Option<&Arc<NestedToolCallRunner>>,
    emit: &EventSink,
) -> ToolResultMessage {
    let mut result = ToolResultMessage {
        tool_call_id: outcome.tool_call.id.clone(),
        tool_name: outcome.tool_call.name.clone(),
        content: outcome.result.content.clone(),
        details: outcome.result.details.clone(),
        usage: outcome.result.usage,
        is_error: outcome.is_error,
        timestamp: now_ms(),
        duration_ms: outcome.duration_ms,
        ..Default::default()
    };
    if let Some(summary) = runner.and_then(|runner| runner.take_record(&outcome.tool_call.id)) {
        result.nested_calls = summary.calls;
        if let Some(usage) = summary.usage {
            result.usage = Some(match &result.usage {
                Some(own) => combine_usage(own, &usage),
                None => usage,
            });
        }
    }
    match crate::agent_loop::emit_message(emit, Message::ToolResult(result.clone()), true).await {
        Message::ToolResult(replaced) => replaced,
        _ => result,
    }
}
