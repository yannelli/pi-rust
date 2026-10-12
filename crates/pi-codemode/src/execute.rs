//! Runs one `codemode` script: nested tool calls through the host, discovery and model globals,
//! the store on the branch, and the formatted result.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use pi_agent::ToolUpdateCallback;
use pi_ai::{ContentBlock, Usage};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::calls::{
    CallLog, CodemodeNestedCall, CodemodeNestedCallStatus, CodemodeToolDetails, ERROR_PREVIEW_CHARS, preview_args,
    truncate_text,
};
use crate::declarations::render_tool_sample;
use crate::description::{codemode_callable_tools, to_codemode_declaration};
use crate::discovery::discovery_globals;
use crate::host::{CodemodeHost, CodemodeToolInfo, NestedToolOutcome};
use crate::js::js_to_fixed;
use crate::loadout::CodemodeToolOptions;
use crate::models::{ModelTotals, model_globals};
use crate::output::{
    DEFAULT_MAX_OUTPUT_TOKENS, format_error, format_output, join_text_runs, save_images, truncate_output, value_text,
};
use crate::sandbox::{CodemodeSandbox, CodemodeSuccess, ExecuteOptions, SandboxFunction, SandboxOptions, Timeout};
use crate::source::{CodemodeSourceError, parse_codemode_source};
use crate::store::CodemodeStoreEntryData;

/// Heap limit of the VM. Overruns throw `InternalError: out of memory` inside the script.
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// The result of one `codemode` call.
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeToolOutput {
    /// `Script completed` or `Script failed` with the wall time, then the output.
    pub content: Vec<ContentBlock>,
    pub details: CodemodeToolDetails,
    /// Usage of the script's `models.*` calls, which counts toward the session cost.
    pub usage: Option<Usage>,
    pub is_error: bool,
}

fn text_of(content: &[ContentBlock]) -> String {
    content.iter().filter_map(ContentBlock::as_text).collect::<Vec<_>>().join("\n")
}

/// What a script receives for a nested call: tools with an output schema resolve to their
/// structured content, also for error results that carry one; other tools to their text.
fn to_script_value(tool: &CodemodeToolInfo, outcome: &NestedToolOutcome) -> Result<Option<Value>, String> {
    if tool.output_schema.is_some()
        && let Some(structured) = &outcome.structured_content
    {
        return Ok(Some(structured.clone()));
    }
    let text = text_of(&outcome.content);
    if outcome.is_error {
        return Err(if text.is_empty() { format!("Tool \"{}\" failed", tool.name) } else { text });
    }
    Ok(Some(Value::String(text)))
}

fn sandbox_tool(
    host: Arc<dyn CodemodeHost>,
    tool: CodemodeToolInfo,
    sample: String,
    parent_id: Arc<str>,
    log: Arc<CallLog>,
) -> SandboxFunction {
    let name = tool.name.clone();
    let tool = Arc::new(tool);
    SandboxFunction::new(name, move |args, cancel| {
        let (host, tool, parent_id, log) = (host.clone(), tool.clone(), parent_id.clone(), log.clone());
        async move {
            let row = log.push(CodemodeNestedCall {
                id: format!("{parent_id}/?"),
                name: tool.name.clone(),
                args: preview_args(args.as_ref()),
                status: CodemodeNestedCallStatus::Running,
                duration_ms: None,
                error: None,
                cost: None,
            });
            let started = Instant::now();
            let outcome = host.execute_tool(&parent_id, &tool.name, args.unwrap_or(Value::Null), cancel.clone()).await;
            log.update(row, |call| {
                call.id = outcome.tool_call_id.clone();
                call.duration_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                if outcome.is_error {
                    call.status = if cancel.is_cancelled() {
                        CodemodeNestedCallStatus::Cancelled
                    } else {
                        CodemodeNestedCallStatus::Error
                    };
                    let text = text_of(&outcome.content);
                    let text = if text.is_empty() { format!("Tool \"{}\" failed", tool.name) } else { text };
                    call.error = Some(truncate_text(&text, ERROR_PREVIEW_CHARS));
                } else {
                    call.status = CodemodeNestedCallStatus::Ok;
                }
            });
            to_script_value(&tool, &outcome)
        }
    })
    .with_description(sample)
}

fn call_summary(calls: &[CodemodeNestedCall]) -> String {
    if calls.is_empty() {
        return "No tool calls were made.".into();
    }
    let names: Vec<String> = calls
        .iter()
        .map(|call| {
            let status = serde_json::to_value(call.status).ok().and_then(|value| value.as_str().map(str::to_string));
            format!("{} ({})", call.name, status.unwrap_or_default())
        })
        .collect();
    format!("Tool calls made before the failure (they are not undone): {}", names.join(", "))
}

/// Run one script. Invalid source (empty input, bad `@options`) fails before anything runs.
pub async fn execute_codemode(
    host: Arc<dyn CodemodeHost>,
    tool_call_id: &str,
    code: &str,
    cancel: CancellationToken,
    on_update: Option<ToolUpdateCallback>,
    options: &CodemodeToolOptions,
) -> Result<CodemodeToolOutput, CodemodeSourceError> {
    let started = Instant::now();
    let parsed = parse_codemode_source(code)?;
    let log = CallLog::new(on_update);
    let totals = Arc::new(ModelTotals::default());

    let callable = Arc::new(codemode_callable_tools(&host.tools()));
    // ALL_TOOLS entries carry the declaration.
    let samples: Arc<HashMap<String, String>> = Arc::new(
        callable
            .iter()
            .map(|tool| {
                (tool.name.clone(), render_tool_sample(&to_codemode_declaration(tool, &tool.prompt_guidelines), None))
            })
            .collect(),
    );
    let parent_id: Arc<str> = Arc::from(tool_call_id);
    let tools = callable
        .iter()
        .map(|tool| {
            let sample = samples.get(&tool.name).cloned().unwrap_or_default();
            sandbox_tool(host.clone(), tool.clone(), sample, parent_id.clone(), log.clone())
        })
        .collect();
    let mut globals = discovery_globals(callable.clone(), samples.clone());
    if options.models && host.models().is_some() {
        globals.extend(model_globals(host.clone(), tool_call_id, &options.docs_path, log.clone(), totals.clone()));
    }
    let timeout = parsed.options.timeout_ms.map_or(Timeout::Never, |ms| Timeout::After(Duration::from_millis(ms)));
    let sandbox = CodemodeSandbox::new(SandboxOptions {
        tools,
        globals,
        timeout,
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
    })
    .map_err(|error| CodemodeSourceError(error.0))?;
    let store = host.store_snapshot();
    let result = sandbox
        .execute(&parsed.code, ExecuteOptions { cancel: Some(cancel), timeout: None, store })
        .await
        .map_err(|error| CodemodeSourceError(error.0))?;
    log.cancel_running();
    let calls = log.snapshot();

    let mut script_output = result.output.clone();
    if let Ok(CodemodeSuccess { value, store_writes }) = &result.outcome {
        if !store_writes.is_empty() {
            host.append_store_entry(CodemodeStoreEntryData::from(store_writes.clone()));
        }
        // A returned value is appended like text().
        if let Some(value) = value {
            script_output.push(crate::sandbox::CodemodeOutputItem::Text { text: value_text(value), console: false });
        }
    }
    let mut items = format_output(&script_output);
    if let Err(error) = &result.outcome {
        items.push(ContentBlock::text(format!("Script error:\n{}", format_error(error, &call_summary(&calls)))));
    }
    let generated = totals.generated_images.load(Ordering::SeqCst);
    if generated > 0 && !items.iter().any(|item| matches!(item, ContentBlock::Image { .. })) {
        let plural = if generated == 1 { "" } else { "s" };
        items.push(ContentBlock::text(format!(
            "Note: models.generateImages() returned {generated} image{plural} that the script did not show. Show each image block of result.output with image(block)."
        )));
    }

    let max_tokens = parsed.options.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let truncated = truncate_output(join_text_runs(items), max_tokens);
    // After truncation, which moves images after the text, so each path stays next to its image.
    let output = join_text_runs(save_images(truncated.items));
    let wall_time = js_to_fixed(started.elapsed().as_secs_f64(), 1);
    let header = if result.is_ok() { "Script completed" } else { "Script failed" };
    let mut content = vec![ContentBlock::text(format!("{header}\nWall time {wall_time} seconds\nOutput:\n"))];
    content.extend(output);
    let usage = *totals.usage.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    Ok(CodemodeToolOutput {
        content,
        details: CodemodeToolDetails { calls, full_output_path: truncated.full_output_path },
        usage,
        is_error: !result.is_ok(),
    })
}
