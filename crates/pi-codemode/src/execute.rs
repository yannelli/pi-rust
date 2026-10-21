//! Runs one `codemode` script: nested tool calls through the host, discovery and model globals,
//! the store on the branch, and the formatted result.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use pi_agent::ToolUpdateCallback;
use pi_ai::{ContentBlock, Usage};
use serde_json::Value;
use tokio::sync::oneshot;
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
use crate::sandbox::{
    AcquireWorker, CodemodeSandbox, CodemodeSuccess, ExecuteOptions, SandboxFunction, SandboxOptions, Timeout,
};
use crate::source::{CodemodeSourceError, parse_codemode_source};
use crate::store::CodemodeStoreEntryData;

/// Heap limit of the VM. Overruns throw `InternalError: out of memory` inside the script.
pub const CODEMODE_MEMORY_LIMIT_BYTES: usize = 256 * 1024 * 1024;

/// Default limit on worker threads across calls and sessions: one replacement while a cancelled
/// worker still exits. `PI_CODEMODE_MAX_WORKERS` raises it for hosts that run many sessions.
pub const MAX_CONCURRENT_CODEMODE_WORKERS: usize = 2;
/// How long a script waits for a free worker before it fails without running.
const CODEMODE_WORKER_WAIT: Duration = Duration::from_secs(60);
const WORKER_LIMIT_ENV: &str = "PI_CODEMODE_MAX_WORKERS";
/// Set by [`set_codemode_worker_limit`]; zero means the environment or the default decides.
static CODEMODE_WORKER_LIMIT: AtomicUsize = AtomicUsize::new(0);

/// Workers alive and the scripts waiting for one, in arrival order.
struct WorkerSlots {
    active: usize,
    next_waiter: u64,
    waiters: VecDeque<(u64, oneshot::Sender<()>)>,
}

static WORKER_SLOTS: Mutex<WorkerSlots> =
    Mutex::new(WorkerSlots { active: 0, next_waiter: 0, waiters: VecDeque::new() });

fn worker_slots() -> MutexGuard<'static, WorkerSlots> {
    WORKER_SLOTS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `PI_CODEMODE_MAX_WORKERS` when it is a positive integer.
fn parse_worker_limit(value: Option<&str>) -> Option<usize> {
    let number = value?.trim().parse::<f64>().ok()?;
    (number.is_finite() && number >= 1.0 && number.fract() == 0.0).then_some(number as usize)
}

/// Read per call so hosts can set the variable at runtime.
fn max_codemode_workers() -> usize {
    match CODEMODE_WORKER_LIMIT.load(Ordering::SeqCst) {
        0 => parse_worker_limit(std::env::var(WORKER_LIMIT_ENV).ok().as_deref())
            .unwrap_or(MAX_CONCURRENT_CODEMODE_WORKERS),
        limit => limit,
    }
}

/// Give free slots to waiting scripts in arrival order.
fn admit_waiting_scripts(slots: &mut WorkerSlots) {
    let limit = max_codemode_workers();
    while slots.active < limit {
        let Some((_, grant)) = slots.waiters.pop_front() else { return };
        slots.active += 1;
        if grant.send(()).is_err() {
            slots.active -= 1;
        }
    }
}

fn release_codemode_worker() {
    let mut slots = worker_slots();
    slots.active = slots.active.saturating_sub(1);
    admit_waiting_scripts(&mut slots);
}

/// Replace the process-wide worker limit, for hosts and test suites that run many scripts at once.
/// Zero restores `PI_CODEMODE_MAX_WORKERS` or the default.
pub fn set_codemode_worker_limit(limit: usize) {
    CODEMODE_WORKER_LIMIT.store(limit, Ordering::SeqCst);
    admit_waiting_scripts(&mut worker_slots());
}

/// Why a script got no worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unreserved {
    Aborted,
    TimedOut,
}

/// A script's place in the queue. Leaving it, by a grant that came too late or by dropping the
/// wait, returns the place and any slot granted meanwhile.
struct QueuedScript {
    id: u64,
    granted: oneshot::Receiver<()>,
}

impl Drop for QueuedScript {
    fn drop(&mut self) {
        worker_slots().waiters.retain(|(id, _)| *id != self.id);
        if self.granted.try_recv().is_ok() {
            release_codemode_worker();
        }
    }
}

/// Reserve one worker slot, waiting in arrival order while all are taken.
async fn reserve_codemode_worker(cancel: &CancellationToken, wait: Duration) -> Result<WorkerReservation, Unreserved> {
    if cancel.is_cancelled() {
        return Err(Unreserved::Aborted);
    }
    let (grant, granted) = oneshot::channel();
    let mut queued = {
        let mut slots = worker_slots();
        let id = slots.next_waiter;
        slots.next_waiter += 1;
        slots.waiters.push_back((id, grant));
        admit_waiting_scripts(&mut slots);
        QueuedScript { id, granted }
    };
    tokio::select! {
        biased;
        result = &mut queued.granted => match result {
            Ok(()) => Ok(WorkerReservation(Arc::new(AtomicBool::new(true)))),
            Err(_) => Err(Unreserved::Aborted),
        },
        _ = cancel.cancelled() => Err(Unreserved::Aborted),
        _ = tokio::time::sleep(wait) => Err(Unreserved::TimedOut),
    }
}

/// The result of a script that waited for a worker in vain.
fn worker_wait_failure(
    why: Unreserved,
    deadline: Option<Duration>,
    started: Instant,
    calls: Vec<CodemodeNestedCall>,
) -> CodemodeToolOutput {
    let error = match (why, deadline) {
        (Unreserved::Aborted, _) => "Script aborted: Execution aborted".to_string(),
        (Unreserved::TimedOut, Some(deadline)) if deadline <= CODEMODE_WORKER_WAIT => format!(
            "Script timed out: Execution timed out after {} ms while waiting for a free codemode worker",
            deadline.as_millis()
        ),
        (Unreserved::TimedOut, _) => format!(
            "No codemode worker became free within {} seconds. All {} workers, shared by every session in this process, are held by other scripts. Retry after they finish.",
            CODEMODE_WORKER_WAIT.as_secs(),
            max_codemode_workers()
        ),
    };
    let wall_time = js_to_fixed(started.elapsed().as_secs_f64(), 1);
    CodemodeToolOutput {
        content: vec![
            ContentBlock::text(format!("Script failed\nWall time {wall_time} seconds\nOutput:\n")),
            ContentBlock::text(format!("Script error:\n{error}\n\nThe script did not start.")),
        ],
        details: CodemodeToolDetails { calls, full_output_path: None },
        usage: None,
        is_error: true,
    }
}

/// The slot reserved for one script. The sandbox takes it when its worker starts and gives it
/// back when the worker exits; a script that never starts a worker releases it here.
struct WorkerReservation(Arc<AtomicBool>);

impl WorkerReservation {
    fn admission(&self) -> AcquireWorker {
        let held = self.0.clone();
        AcquireWorker(Arc::new(move || {
            if !held.swap(false, Ordering::SeqCst) {
                return Err("The script already settled".to_string());
            }
            Ok(Box::new(release_codemode_worker))
        }))
    }
}

impl Drop for WorkerReservation {
    fn drop(&mut self) {
        if self.0.swap(false, Ordering::SeqCst) {
            release_codemode_worker();
        }
    }
}

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
    // The wait for a worker counts toward the script's deadline.
    let deadline = parsed.options.timeout_ms.map(Duration::from_millis);
    let wait_started = Instant::now();
    let wait = deadline.map_or(CODEMODE_WORKER_WAIT, |deadline| deadline.min(CODEMODE_WORKER_WAIT));
    let reservation = match reserve_codemode_worker(&cancel, wait).await {
        Ok(reservation) => reservation,
        Err(why) => return Ok(worker_wait_failure(why, deadline, started, log.snapshot())),
    };
    let waited = wait_started.elapsed();
    let timeout = deadline.map_or(Timeout::Never, |deadline| {
        Timeout::After(deadline.saturating_sub(waited).max(Duration::from_millis(1)))
    });
    let sandbox = CodemodeSandbox::new(SandboxOptions {
        tools,
        globals,
        timeout,
        memory_limit_bytes: Some(CODEMODE_MEMORY_LIMIT_BYTES),
        acquire_worker: Some(reservation.admission()),
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

#[cfg(test)]
mod tests {
    use super::parse_worker_limit;

    #[test]
    fn worker_limit_comes_from_a_positive_integer() {
        assert_eq!(parse_worker_limit(Some("3")), Some(3));
        assert_eq!(parse_worker_limit(Some(" 12 ")), Some(12));
        assert_eq!(parse_worker_limit(Some("4.0")), Some(4));
        for invalid in ["0", "-1", "2.5", "", "many", "NaN", "inf"] {
            assert_eq!(parse_worker_limit(Some(invalid)), None, "{invalid:?}");
        }
        assert_eq!(parse_worker_limit(None), None);
    }
}
