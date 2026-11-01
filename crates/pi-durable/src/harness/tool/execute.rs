//! The `call` and `execute` phases and the abort handler of the tool task, and one execution of a tool.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pi_ai::{ContentBlock, JsonObject};
use pi_chord::Context;

use super::api::{CallState, Reported, ToolExecutionApi};
use super::result::{
    bound_content, from_slot, harness_error, output_value, structured_output_error, tool_diagnostic, truncated,
};
use super::settle::{Ending, publish_progress, settle};
use super::{
    BeforeToolDecision, ToolHookCall, ToolHooks, ToolTaskCheckpoint, ToolTaskInput, decode_input, json_equal, prepare,
    read_call, resolve_tool, validate,
};
use crate::documents::encode;
use crate::env::ShellOutputWindow;
use crate::errors::{DurableError, DurableResult};
use crate::harness::agent::tool_extension;
use crate::harness::live::{LIVE_DOC, SlotRef, SlotStatus, tool_slot};
use crate::harness::output::{OutputBuffer, OutputLimits};
use crate::harness::progress::PROGRESS_BYTES_PER_SECOND;
use crate::harness::scheduler::TaskRuntime;
use crate::harness::tool_registration::{Retain, ToolRegistration, ToolReplay};
use crate::harness::types::{DiagnosticSeverity, ToolExecutionResult};
use crate::harness::util::lock;
use crate::session::Tx;
use crate::truncate::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES};
use crate::types::{AbortReason, ReportFrom, TaskRecord, TaskState};

fn invalid(message: &str) -> ToolExecutionResult {
    harness_error("invalid_arguments", message)
}

pub(super) async fn call(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let input = decode_input(&task)?;
    let call = read_call(&runtime, &input, &context).await?;
    if crate::harness::live::cut_off_too_often(&task, &runtime) {
        return end_cut_off(&task, &runtime, &input, &call, &context).await;
    }
    let Some(tool) = resolve_tool(&runtime, &input, &call.call.name, &context).await? else {
        let error = harness_error("tool_unavailable", &format!("Tool {} is not available", call.call.name));
        return settle(&runtime, &input, &call, Ending::Completed, move |_| error, &context, None).await;
    };
    let checked = prepare(&tool, &call.call.arguments).and_then(|args| validate(&tool, &call.call, args));
    let mut args = match checked {
        Ok(args) => args,
        Err(message) => {
            return settle(&runtime, &input, &call, Ending::Completed, move |_| invalid(&message), &context, None)
                .await;
        }
    };
    let mut block: Option<String> = None;
    for hooks in runtime.hooks::<ToolHooks>(&context).await? {
        let Some(hook) = &hooks.before_tool else { continue };
        if block.is_some() {
            break;
        }
        let seen = ToolHookCall {
            call: pi_ai::ToolCall { arguments: args.clone(), ..call.call.clone() },
            parent: call.parent.clone(),
        };
        match hook(seen, runtime.clone(), context.clone()).await {
            Ok(Some(BeforeToolDecision { block: Some(reason), .. })) => block = Some(reason),
            Ok(Some(BeforeToolDecision { arguments: Some(replaced), .. })) => args = replaced,
            Ok(_) => {}
            Err(error) => {
                if runtime.signal().aborted() {
                    return Err(error);
                }
                block = Some(error.to_string());
            }
        }
    }
    if let Some(reason) = block {
        let blocked = harness_error("blocked", &format!("Tool call blocked: {reason}"));
        return settle(&runtime, &input, &call, Ending::Completed, move |_| blocked, &context, None).await;
    }
    let args = match validate(&tool, &call.call, args) {
        Ok(args) => args,
        Err(message) => {
            return settle(&runtime, &input, &call, Ending::Completed, move |_| invalid(&message), &context, None)
                .await;
        }
    };
    let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
    let replay = tool.replay.unwrap_or_default();
    // A nested slot shows the arguments the call runs with, which repair, hooks, and coercion may change.
    let changed = match &input {
        ToolTaskInput::Nested { call, .. } => !json_equal(&call.arguments, &args),
        ToolTaskInput::Model { .. } => false,
    };
    let intent = args.clone();
    let change = move |tx: Tx, _| async move {
        tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| match tool_slot(live, task_id) {
            Some(SlotRef::Nested(slot)) => {
                slot.status = SlotStatus::Running;
                if changed {
                    slot.arguments = pi_chord::Json::from(serde_json::Value::Object(intent.clone()))
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                }
            }
            Some(mut slot) => slot.set_status(SlotStatus::Running),
            None => {}
        })?;
        let checkpoint = ToolTaskCheckpoint::Execute { arguments: intent, replay };
        Ok(Some(TaskState::Running { checkpoint: encode(&checkpoint)? }))
    };
    runtime.commit(change, &context).await?;
    run(&runtime, &input, &call, tool, args, &context).await
}

/// Recovery after intent: rerun only when the stored and the current policy both say `safe`.
pub(super) async fn execute(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let input = decode_input(&task)?;
    let checkpoint = task.state.checkpoint().cloned().unwrap_or(pi_chord::Json::Null);
    let ToolTaskCheckpoint::Execute { arguments, replay } = checkpoint.deserialize_into()? else {
        return Err(DurableError::error("Tool checkpoint is not in phase execute"));
    };
    let call = read_call(&runtime, &input, &context).await?;
    let tool = resolve_tool(&runtime, &input, &call.call.name, &context).await?;
    if let Some(tool) = tool.filter(|tool| replay == ToolReplay::Safe && tool.replay == Some(ToolReplay::Safe)) {
        if crate::harness::live::cut_off_too_often(&task, &runtime) {
            return end_cut_off(&task, &runtime, &input, &call, &context).await;
        }
        // The rerun reports from scratch; clear what the interrupted run published.
        let (conversation_id, task_id) = (runtime.conversation_id(), runtime.task_id());
        let change = move |tx: Tx, _| async move {
            tx.doc(&LIVE_DOC, conversation_id).await?.edit(|live| {
                if let Some(mut slot) = tool_slot(live, task_id) {
                    slot.clear_progress();
                }
            })?;
            Ok(None)
        };
        runtime.commit(change, &context).await?;
        return run(&runtime, &input, &call, tool, arguments, &context).await;
    }
    let message = format!("Tool {} was interrupted and may have partially run", call.call.name);
    // `failed` records cancellation intent, so the call's owned conversations, left unsupervised, are aborted.
    let ending = Ending::Failed { message: message.clone(), reason: None };
    settle(&runtime, &input, &call, ending, move |slot| from_slot(slot, "interrupted", &message), &context, None).await
}

async fn end_cut_off(
    task: &TaskRecord,
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    context: &Context,
) -> DurableResult<()> {
    let count = crate::harness::live::cut_off_count(task);
    let message = format!("Tool {} was interrupted {count}; it is not run again", call.call.name);
    let ending = Ending::Failed { message: message.clone(), reason: Some("restart_limit".into()) };
    settle(runtime, input, call, ending, move |slot| from_slot(slot, "restart_limit", &message), context, None).await
}

pub(super) async fn abort(task: TaskRecord, runtime: TaskRuntime, context: Context) -> DurableResult<()> {
    let input = decode_input(&task)?;
    let call = read_call(&runtime, &input, &context).await?;
    let name = &call.call.name;
    let executing = task.state.checkpoint().and_then(|c| c.get("phase")).and_then(|p| p.as_str()) == Some("execute");
    let (code, message) = match (task.abort_reason, executing) {
        (Some(AbortReason::Restart), true) => {
            ("interrupted", format!("Tool {name} was interrupted by a restart and may have partially run"))
        }
        (Some(AbortReason::Restart), false) => {
            ("abandoned", format!("Tool {name} was not started: its caller ended with a restart"))
        }
        (None, _) => ("aborted", format!("Tool {name} was aborted")),
    };
    settle(&runtime, &input, &call, Ending::Aborted, move |slot| from_slot(slot, code, &message), &context, None).await
}

/// Execute with the resolved implementation, then settle its result.
async fn run(
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: Arc<ToolRegistration>,
    args: JsonObject,
    context: &Context,
) -> DurableResult<()> {
    let limits = tool.output_limits.unwrap_or_default();
    let limits = OutputLimits {
        max_bytes: limits.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
        max_lines: limits.max_lines.unwrap_or(DEFAULT_MAX_LINES),
        retain: limits.retain.unwrap_or_default(),
    };
    let reported = Reported { output: OutputBuffer::new(limits), limits, diagnostics: Vec::new(), details: None };
    let state = Arc::new(CallState {
        reported: Mutex::new(reported),
        ended: AtomicBool::new(false),
        admissions: Mutex::new(Vec::new()),
        sequence: AtomicU32::new(0),
        made_nested: Mutex::new(None),
        resumes: tool.replay == Some(ToolReplay::Safe),
    });
    let streams = !matches!(input, ToolTaskInput::Nested { progress: Some(false), .. });
    let progress = publish_progress(runtime, state.clone(), streams);
    let output_window = (limits.retain == Retain::Tail).then(|| ShellOutputWindow {
        max_bytes: limits.max_bytes as u64,
        max_lines: limits.max_lines as u64,
        min_interval_ms: runtime.settings().progress.output_interval_ms as f64,
        bytes_per_second: PROGRESS_BYTES_PER_SECOND,
    });
    let mut duration_ms = None;
    let executed = async {
        // Built for this call, so a rerun after recovery gets the conversation's environment at that time.
        let env = runtime.env(context).await?;
        let api = ToolExecutionApi {
            task_id: runtime.task_id(),
            conversation_id: runtime.conversation_id(),
            call_id: call.call.id.clone(),
            env,
            output_window,
            runtime: runtime.clone(),
            state: state.clone(),
            progress: progress.clone(),
        };
        let started = Instant::now();
        let result = (tool.execute)(args, api, context.clone()).await;
        duration_ms = Some(started.elapsed().as_millis() as u64);
        result
    }
    .await;
    let (result, ending) = match executed {
        Ok(result) => (result, Ending::Completed),
        Err(error) => {
            if runtime.signal().aborted() {
                state.ended.store(true, Ordering::SeqCst);
                for waiter in progress.stop().await {
                    let _ = waiter.send(Err(error.clone()));
                }
                return Err(error);
            }
            let result = ToolExecutionResult {
                is_error: Some(true),
                diagnostics: Some(vec![tool_diagnostic("tool_error", &error.to_string())]),
                ..ToolExecutionResult::default()
            };
            // A failure ends the task `failed`, which cancels what the call owned.
            (result, Ending::Failed { message: format!("Tool {} threw", call.call.name), reason: None })
        }
    };
    state.ended.store(true, Ordering::SeqCst);
    // No admission starts after the end; let those underway commit, so settlement aborts their calls.
    let admissions = std::mem::take(&mut *lock(&state.admissions));
    for admission in admissions {
        let _ = admission.await;
    }
    lock(&state.reported).output.end();
    let pending = progress.stop().await;
    let settled = async {
        let result = final_result(runtime, input, call, &tool, result, &state, context).await?;
        settle(runtime, input, call, ending, move |_| result, context, duration_ms).await
    }
    .await;
    for waiter in pending {
        let _ = waiter.send(settled.clone());
    }
    settled
}

/// The settled result: fallbacks from what the tool reported, `after_tool` applied, text bounded, and
/// `structured_output` checked against the tool's schema, with the Harness's diagnostics last.
async fn final_result(
    runtime: &TaskRuntime,
    input: &ToolTaskInput,
    call: &ToolHookCall,
    tool: &ToolRegistration,
    result: ToolExecutionResult,
    state: &CallState,
    context: &Context,
) -> DurableResult<ToolExecutionResult> {
    let mut harness = Vec::new();
    let (kept, limits, details, diagnostics) = {
        let mut reported = lock(&state.reported);
        let kept = result.output.is_none().then(|| reported.output.snapshot());
        (kept, reported.limits, reported.details.clone(), reported.diagnostics.clone())
    };
    let output: Vec<ContentBlock> = match (&result.output, &kept) {
        (Some(output), _) => output.clone(),
        (None, Some(kept)) if !kept.text.is_empty() => vec![ContentBlock::text(kept.text.clone())],
        _ => Vec::new(),
    };
    let mut all = diagnostics;
    all.extend(result.diagnostics.clone().unwrap_or_default());
    let mut last = ToolExecutionResult {
        output: Some(output.clone()),
        details: result.details.clone().or(details),
        diagnostics: Some(all),
        ..result
    };
    for hooks in runtime.hooks::<ToolHooks>(context).await? {
        let Some(hook) = &hooks.after_tool else { continue };
        match hook((call.clone(), last.clone()), runtime.clone(), context.clone()).await {
            Ok(Some(replaced)) => last = replaced,
            Ok(None) => {}
            Err(error) => runtime.hook_failed(&hooks, error)?,
        }
    }
    // The kept output's truncation applies only while `after_tool` kept that output.
    if let Some(kept) = &kept
        && kept.dropped_bytes > 0
        && last.output.as_ref() == Some(&output)
    {
        harness.push(truncated(kept.dropped_bytes, kept.dropped_lines, Some(limits.retain)));
    }
    let (content, dropped_bytes, dropped_lines) = bound_content(last.output.as_deref().unwrap_or_default(), limits);
    if dropped_bytes > 0 {
        harness.push(truncated(dropped_bytes, dropped_lines, Some(limits.retain)));
    }
    let nested = matches!(input, ToolTaskInput::Nested { .. });
    if let Some(broken) = structured_output_error(tool, &last) {
        last.structured_output = None;
        if nested {
            last.is_error = Some(true);
            harness.push(tool_diagnostic("invalid_structured_output", &broken));
        } else {
            // The model never sees structured output, so its call stands; the host learns of the broken tool. Only names
            // the extension: a failed resolution leaves the report without the name.
            let agent = runtime.agent(context).await.ok();
            let extension = agent.and_then(|agent| tool_extension(&agent, tool.name()));
            runtime.report_from(&DurableError::error(broken), ReportFrom::Extension(extension));
        }
    }
    if nested && tool.structured_output_schema.is_none() {
        last.structured_output = Some(output_value(&content)?);
    }
    // A schema tool's error result without structured output has its text only in the output, which programs never
    // get: it becomes an error diagnostic, as a thrown error's message does, unless one says what went wrong already.
    if nested
        && tool.structured_output_schema.is_some()
        && last.is_error == Some(true)
        && last.structured_output.is_none()
    {
        let mut diagnostics = last.diagnostics.iter().flatten().chain(&harness);
        if !diagnostics.any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error) {
            let text = content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n");
            if !text.is_empty() {
                harness.push(tool_diagnostic("tool_error", &text));
            }
        }
    }
    let mut diagnostics = last.diagnostics.take().unwrap_or_default();
    diagnostics.extend(harness);
    Ok(ToolExecutionResult { output: Some(content), diagnostics: Some(diagnostics), ..last })
}
