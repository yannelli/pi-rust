//! Tool calls a tool makes while it runs ([`ToolContext::execute_tool`]). Each runs through the
//! tool pipeline with the loop's hooks, emits events with `parent_tool_call_id`, and is recorded
//! on the caller's result message (`nested_calls`). Port of upstream `nested-tool-calls.ts`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures::future::BoxFuture;
use pi_ai::{Cost, NestedToolCallRecord, NestedToolCalls, ToolCall, Usage};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::types::{
    AgentEvent, AgentToolCallOutcome, AgentToolResult, SharedTool, ToolExecutionMode, ToolUpdateCallback,
};

/// Limits of the nested-call record: oversized arguments are omitted, calls beyond the count
/// are dropped, and the record is marked incomplete when either happens.
#[derive(Clone, Copy, Debug)]
pub struct NestedCallLimits {
    pub max_calls: usize,
    pub max_argument_bytes_per_call: usize,
    pub max_argument_bytes_total: usize,
    pub max_error_chars: usize,
}

pub const NESTED_CALL_LIMITS: NestedCallLimits = NestedCallLimits {
    max_calls: 256,
    max_argument_bytes_per_call: 8 * 1024,
    max_argument_bytes_total: 32 * 1024,
    max_error_chars: 500,
};

/// Sum of two usages, including cost.
pub fn combine_usage(first: &Usage, second: &Usage) -> Usage {
    let optional =
        |a: Option<u64>, b: Option<u64>| (a.is_some() || b.is_some()).then(|| a.unwrap_or(0) + b.unwrap_or(0));
    Usage {
        input: first.input + second.input,
        output: first.output + second.output,
        cache_read: first.cache_read + second.cache_read,
        cache_write: first.cache_write + second.cache_write,
        cache_write_1h: optional(first.cache_write_1h, second.cache_write_1h),
        reasoning: optional(first.reasoning, second.reasoning),
        total_tokens: first.total_tokens + second.total_tokens,
        cost: Cost {
            input: first.cost.input + second.cost.input,
            output: first.cost.output + second.cost.output,
            cache_read: first.cost.cache_read + second.cost.cache_read,
            cache_write: first.cost.cache_write + second.cost.cache_write,
            total: first.cost.total + second.cost.total,
        },
    }
}

/// The nested calls of one model-issued call, including calls made by nested tools.
#[derive(Debug)]
pub struct NestedCallRecorder {
    calls: Vec<NestedToolCallRecord>,
    started: Vec<Option<Instant>>,
    complete: bool,
    argument_bytes: usize,
    /// Summed usage of every nested result, including calls dropped from the record.
    usage: Option<Usage>,
}

impl Default for NestedCallRecorder {
    fn default() -> Self {
        Self { calls: Vec::new(), started: Vec::new(), complete: true, argument_bytes: 0, usage: None }
    }
}

impl NestedCallRecorder {
    /// Record a call as it starts. Returns its index, or `None` when the call is dropped.
    pub fn start(&mut self, call: &ToolCall) -> Option<usize> {
        if self.calls.len() >= NESTED_CALL_LIMITS.max_calls {
            self.complete = false;
            return None;
        }
        let mut record = NestedToolCallRecord {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments: None,
            arguments_bytes: None,
            status: "unfinished".into(),
            duration_ms: None,
            error: None,
        };
        let bytes = serde_json::to_string(&call.arguments).map(|json| json.len()).unwrap_or_default();
        if bytes > NESTED_CALL_LIMITS.max_argument_bytes_per_call
            || self.argument_bytes + bytes > NESTED_CALL_LIMITS.max_argument_bytes_total
        {
            record.arguments_bytes = Some(bytes as u64);
            self.complete = false;
        } else {
            record.arguments = Some(call.arguments.clone());
            self.argument_bytes += bytes;
        }
        self.calls.push(record);
        self.started.push(Some(Instant::now()));
        Some(self.calls.len() - 1)
    }

    pub fn finish(&mut self, index: Option<usize>, is_error: bool, error_text: &str) {
        let Some(index) = index else { return };
        let started = self.started[index].take();
        let record = &mut self.calls[index];
        record.status = if is_error { "error" } else { "ok" }.into();
        record.duration_ms = Some(started.map(|started| started.elapsed().as_millis() as u64).unwrap_or_default());
        if is_error && !error_text.is_empty() {
            record.error = Some(error_text.chars().take(NESTED_CALL_LIMITS.max_error_chars).collect());
        }
    }

    pub fn add_usage(&mut self, usage: &Usage) {
        self.usage = Some(match &self.usage {
            Some(total) => combine_usage(total, usage),
            None => *usage,
        });
    }

    pub fn total_usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Copy of the record so far, or `None` when no nested call was made.
    pub fn snapshot(&self) -> Option<NestedToolCalls> {
        if self.calls.is_empty() && self.complete {
            return None;
        }
        let complete = self.complete && self.calls.iter().all(|call| call.status != "unfinished");
        Some(NestedToolCalls { calls: self.calls.clone(), complete })
    }
}

/// What the nested calls of one model-issued call leave on its tool result message.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NestedCallSummary {
    /// Becomes `nested_calls`. `None` when no nested call was made.
    pub calls: Option<NestedToolCalls>,
    /// Summed usage of the nested results, added to the message's `usage`.
    pub usage: Option<Usage>,
}

/// Options for [`ToolContext::execute_tool`].
#[derive(Clone, Default)]
pub struct ExecuteToolOptions {
    /// Defaults to the calling tool's token.
    pub cancel: Option<CancellationToken>,
    /// Receives partial results of the nested tool, besides `tool_execution_update` events.
    pub on_update: Option<ToolUpdateCallback>,
}

/// Runs nested calls through the tool pipeline and reports their events.
#[async_trait]
pub trait NestedToolCallHost: Send + Sync {
    /// Tools nested calls resolve against.
    fn tools(&self) -> Vec<SharedTool>;
    /// Whether every nested call runs exclusively, as when the agent runs tool calls in order.
    fn is_sequential(&self) -> bool;
    /// Run the call with hooks that report `parent_tool_call_id`.
    async fn run_tool_call(
        &self,
        call: ToolCall,
        parent_tool_call_id: &str,
        context: ToolContext,
        cancel: CancellationToken,
        on_update: ToolUpdateCallback,
    ) -> AgentToolCallOutcome;
    fn emit(&self, event: AgentEvent);
}

/// Calls below one model-issued call share its recorder.
struct CallScope {
    recorder: Arc<Mutex<NestedCallRecorder>>,
    next_id: u64,
    /// Set inside a call that holds the exclusive queue, so its own nested calls do not wait on it.
    holds_queue: bool,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs nested calls and keeps their records until the calling call's result is built.
pub struct NestedToolCallRunner {
    host: Arc<dyn NestedToolCallHost>,
    scopes: Mutex<HashMap<String, CallScope>>,
    /// Serializes nested calls that must not run concurrently.
    queue: tokio::sync::Mutex<()>,
}

fn text_of(result: &AgentToolResult) -> String {
    result.content.iter().filter_map(pi_ai::ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

impl NestedToolCallRunner {
    pub fn new(host: Arc<dyn NestedToolCallHost>) -> Arc<Self> {
        Arc::new(Self { host, scopes: Mutex::new(HashMap::new()), queue: tokio::sync::Mutex::new(()) })
    }

    pub fn tools(&self) -> Vec<SharedTool> {
        self.host.tools()
    }

    /// Run `name` for the call `caller_id`; the nested call gets the id `<caller_id>/<n>`.
    /// Tool failures come back as `is_error` outcomes.
    pub fn execute(
        self: Arc<Self>,
        caller_id: String,
        name: String,
        args: Value,
        options: ExecuteToolOptions,
        caller_cancel: CancellationToken,
    ) -> BoxFuture<'static, AgentToolCallOutcome> {
        Box::pin(async move {
            let (recorder, id, holds_queue) = {
                let mut scopes = lock(&self.scopes);
                let scope = scopes.entry(caller_id.clone()).or_insert_with(|| CallScope {
                    recorder: Arc::default(),
                    next_id: 1,
                    holds_queue: false,
                });
                let id = format!("{caller_id}/{}", scope.next_id);
                scope.next_id += 1;
                (scope.recorder.clone(), id, scope.holds_queue)
            };
            let arguments = match args {
                Value::Object(object) => object,
                _ => Default::default(),
            };
            let call = ToolCall { id: id.clone(), name: name.clone(), arguments, ..Default::default() };
            let index = lock(&recorder).start(&call);
            let call_args = Value::Object(call.arguments.clone());
            self.host.emit(AgentEvent::ToolExecutionStart {
                tool_call_id: id.clone(),
                tool_name: name.clone(),
                args: call_args.clone(),
                parent_tool_call_id: Some(caller_id.clone()),
            });

            let exclusive = !holds_queue
                && (self.host.is_sequential()
                    || self.host.tools().iter().any(|tool| {
                        tool.name() == name && tool.execution_mode() == Some(ToolExecutionMode::Sequential)
                    }));
            let guard = if exclusive { Some(self.queue.lock().await) } else { None };
            lock(&self.scopes).insert(
                id.clone(),
                CallScope { recorder: recorder.clone(), next_id: 1, holds_queue: holds_queue || exclusive },
            );
            let on_update: ToolUpdateCallback = {
                let (host, id, name, args, parent) =
                    (self.host.clone(), id.clone(), name.clone(), call_args, caller_id.clone());
                let forward = options.on_update.clone();
                Arc::new(move |partial_result: AgentToolResult| {
                    if let Some(forward) = &forward {
                        forward(partial_result.clone());
                    }
                    host.emit(AgentEvent::ToolExecutionUpdate {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                        partial_result,
                        parent_tool_call_id: Some(parent.clone()),
                    });
                })
            };
            let cancel = options.cancel.unwrap_or(caller_cancel);
            let context = ToolContext::nested(id.clone(), self.clone(), cancel.clone());
            let outcome = self.host.run_tool_call(call, &caller_id, context, cancel, on_update).await;
            lock(&self.scopes).remove(&id);
            drop(guard);

            {
                let mut recorder = lock(&recorder);
                recorder.finish(index, outcome.is_error, &text_of(&outcome.result));
                // Nested results are not persisted, so their usage only counts through the record.
                if let Some(usage) = &outcome.result.usage {
                    recorder.add_usage(usage);
                }
            }
            self.host.emit(AgentEvent::ToolExecutionEnd {
                tool_call_id: id,
                tool_name: name,
                result: outcome.result.clone(),
                is_error: outcome.is_error,
                duration_ms: outcome.duration_ms,
                parent_tool_call_id: Some(caller_id),
            });
            outcome
        })
    }

    /// Remove and return the record of the nested calls a model-issued call made.
    pub fn take_record(&self, tool_call_id: &str) -> Option<NestedCallSummary> {
        let scope = lock(&self.scopes).remove(tool_call_id)?;
        let recorder = lock(&scope.recorder);
        Some(NestedCallSummary { calls: recorder.snapshot(), usage: recorder.total_usage() })
    }

    pub fn clear(&self) {
        lock(&self.scopes).clear();
    }
}

/// What a running tool knows about its call: its id and, inside the agent loop, a way to run
/// other tools through the same validation, hooks, and permission checks as model calls.
#[derive(Clone)]
pub struct ToolContext {
    tool_call_id: String,
    runner: Option<Arc<NestedToolCallRunner>>,
    cancel: CancellationToken,
}

impl ToolContext {
    /// A context without nested calls, for tools run outside the loop.
    pub fn new(tool_call_id: impl Into<String>) -> Self {
        Self { tool_call_id: tool_call_id.into(), runner: None, cancel: CancellationToken::new() }
    }

    pub fn nested(
        tool_call_id: impl Into<String>,
        runner: Arc<NestedToolCallRunner>,
        cancel: CancellationToken,
    ) -> Self {
        Self { tool_call_id: tool_call_id.into(), runner: Some(runner), cancel }
    }

    pub fn tool_call_id(&self) -> &str {
        &self.tool_call_id
    }

    /// Whether [`execute_tool`](Self::execute_tool) can run tools.
    pub fn can_execute_tools(&self) -> bool {
        self.runner.is_some()
    }

    /// Tools [`execute_tool`](Self::execute_tool) can call.
    pub fn tools(&self) -> Vec<SharedTool> {
        self.runner.as_ref().map(|runner| runner.tools()).unwrap_or_default()
    }

    /// Run another tool. It gets the id `<calling id>/<n>`, its events carry
    /// `parent_tool_call_id`, and a bounded record lands in the caller's `nested_calls`.
    pub async fn execute_tool(&self, name: &str, args: Value, options: ExecuteToolOptions) -> AgentToolCallOutcome {
        match &self.runner {
            Some(runner) => {
                runner.clone().execute(self.tool_call_id.clone(), name.into(), args, options, self.cancel.clone()).await
            }
            None => AgentToolCallOutcome {
                tool_call: ToolCall {
                    id: format!("{}/0", self.tool_call_id),
                    name: name.into(),
                    arguments: args.as_object().cloned().unwrap_or_default(),
                    ..Default::default()
                },
                result: AgentToolResult::error("Nested tool calls are not available outside the agent loop"),
                is_error: true,
                duration_ms: None,
            },
        }
    }
}
