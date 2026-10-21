//! Runs JavaScript in an embedded QuickJS VM on its own thread. Scripts see `tools.<name>(args)`,
//! `ALL_TOOLS`, `text`, `image`, `exit`, `console.*`, `store`/`load`, and the configured globals;
//! nothing else (no timers, network, file system, or modules).

mod execution;
mod types;
mod worker;

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use types::*;

use crate::identifier::{is_identifier, to_codemode_identifier};

/// Default deadline of one execution.
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const RESERVED_GLOBALS: [&str; 9] =
    ["tools", "ALL_TOOLS", "console", "text", "image", "exit", "globalThis", "store", "load"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxError(pub String);

impl fmt::Display for SandboxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SandboxError {}

#[derive(Clone, Debug)]
pub struct SandboxOptions {
    pub tools: Vec<SandboxFunction>,
    /// Functions exposed as top-level identifiers or `namespace.member`. The result's `calls` list
    /// leaves them out. Names may not shadow the built-in globals.
    pub globals: Vec<SandboxFunction>,
    /// Default deadline per execution.
    pub timeout: Timeout,
    /// Heap limit of the VM. Allocations beyond it throw `InternalError: out of memory` in the script.
    pub memory_limit_bytes: Option<usize>,
    /// Reserves capacity right before each worker thread starts; see [`AcquireWorker`].
    pub acquire_worker: Option<AcquireWorker>,
}

/// Runs when a worker thread exits, or when it fails to start.
pub type WorkerRelease = Box<dyn FnOnce() + Send>;

/// Admission for worker threads, which can be shared across sandboxes. An error denies the worker
/// and fails the execution as a sandbox error. The release runs when the thread actually exits,
/// which can be after the result: a cancelled script stops only at its next interrupt check.
#[derive(Clone)]
pub struct AcquireWorker(pub Arc<dyn Fn() -> Result<WorkerRelease, String> + Send + Sync>);

impl fmt::Debug for AcquireWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AcquireWorker")
    }
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: Timeout::After(Duration::from_millis(DEFAULT_TIMEOUT_MS)),
            memory_limit_bytes: None,
            acquire_worker: None,
        }
    }
}

/// Checks global names: identifiers or `namespace.member`, not shadowing a built-in global or each other.
fn check_globals<'a>(globals: impl IntoIterator<Item = &'a str>) -> Result<(), SandboxError> {
    let mut names: HashSet<&str> = HashSet::new();
    let mut namespaces = Vec::new();
    for name in globals {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() > 2 || !parts.iter().all(|part| is_identifier(part)) || RESERVED_GLOBALS.contains(&parts[0]) {
            return Err(SandboxError(format!("Invalid global name \"{name}\"")));
        }
        if !names.insert(name) {
            return Err(SandboxError(format!("Global \"{name}\" is already registered")));
        }
        if parts.len() == 2 {
            namespaces.push(parts[0]);
        }
    }
    if let Some(name) = namespaces.iter().find(|name| names.contains(*name)) {
        return Err(SandboxError(format!("Global \"{name}\" conflicts with the namespace \"{name}\"")));
    }
    Ok(())
}

/// Holds the tool table and defaults. Each [`execute`](Self::execute) gets its own thread and VM.
pub struct CodemodeSandbox {
    tools: Vec<SandboxFunction>,
    globals: Vec<SandboxFunction>,
    timeout: Timeout,
    memory_limit_bytes: Option<usize>,
    acquire_worker: Option<AcquireWorker>,
    closed: CancellationToken,
}

impl CodemodeSandbox {
    pub fn new(options: SandboxOptions) -> Result<Self, SandboxError> {
        let mut sandbox = Self {
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: options.timeout,
            memory_limit_bytes: options.memory_limit_bytes,
            acquire_worker: options.acquire_worker,
            closed: CancellationToken::new(),
        };
        for tool in options.tools {
            sandbox.register_tool(tool)?;
        }
        check_globals(options.globals.iter().map(SandboxFunction::name))?;
        sandbox.globals = options.globals;
        Ok(sandbox)
    }

    /// Fails if a tool with the same name is already registered.
    pub fn register_tool(&mut self, tool: SandboxFunction) -> Result<(), SandboxError> {
        if self.tools.iter().any(|existing| existing.name() == tool.name()) {
            return Err(SandboxError(format!("Tool \"{}\" is already registered", tool.name())));
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn unregister_tool(&mut self, name: &str) -> bool {
        let before = self.tools.len();
        self.tools.retain(|tool| tool.name() != name);
        self.tools.len() != before
    }

    pub fn tools(&self) -> &[SandboxFunction] {
        &self.tools
    }

    pub fn globals(&self) -> &[SandboxFunction] {
        &self.globals
    }

    /// `code` is an async function body: `return` and top-level `await` work. Script failures come
    /// back as a result with an error; only a closed sandbox fails the call.
    pub async fn execute(&self, code: &str, options: ExecuteOptions) -> Result<CodemodeResult, SandboxError> {
        if self.closed.is_cancelled() {
            return Err(SandboxError("Sandbox is closed".into()));
        }
        let declared_tools: Vec<CodemodeDeclaredTool> = self
            .tools
            .iter()
            .map(|tool| CodemodeDeclaredTool {
                name: tool.name().to_string(),
                js_name: None,
                description: tool.declaration.description.clone(),
            })
            .collect();
        let declared_globals: Vec<CodemodeDeclaredGlobal> = self
            .globals
            .iter()
            .map(|global| CodemodeDeclaredGlobal { name: global.name().to_string(), spread: global.spread })
            .collect();
        let (tools_json, globals_json) = execution::declarations_json(&declared_tools, &declared_globals);
        Ok(execution::run(execution::ExecutionParams {
            code: code.into(),
            tools: self.tools.clone(),
            globals: self.globals.clone(),
            tools_json,
            globals_json,
            timeout: options.timeout.unwrap_or(self.timeout),
            cancel: options.cancel,
            closed: self.closed.clone(),
            memory_limit: self.memory_limit_bytes,
            acquire_worker: self.acquire_worker.clone(),
            store: options.store,
            control: None,
        })
        .await)
    }

    /// Run `code` deterministically, with the caller answering its calls: the same script, start time,
    /// seed, tools, globals, and store, answered with the same results in the same `seq` order, makes
    /// the same calls with the same arguments and produces the same output, on any VM with the same
    /// fingerprint. A host can therefore replay it after a restart. The VM runs in UTC, its clock reads
    /// `started_at` and then the `now` of the result being processed, its randomness comes from `seed`,
    /// and `WeakRef` and `FinalizationRegistry` do not exist.
    ///
    /// The sandbox's own tools and globals are not used: the run declares its own, and every call goes
    /// to `options.on_call`. The run starts at once on the current Tokio runtime; dropping
    /// [`CodemodeRun::result`] ends it. Script failures come back as a result with an error; cancel
    /// through `options.cancel` to end the run with an error the script cannot catch.
    pub fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError> {
        if self.closed.is_cancelled() {
            return Err(SandboxError("Sandbox is closed".into()));
        }
        if !options.started_at.is_finite() || options.started_at < 0.0 {
            return Err(SandboxError("startedAt must be a non-negative number of milliseconds since the epoch".into()));
        }
        let names: HashSet<&str> = options.tools.iter().map(|tool| tool.name.as_str()).collect();
        if names.len() != options.tools.len() {
            return Err(SandboxError("Tool names must be unique".into()));
        }
        let script_names: HashSet<String> = options
            .tools
            .iter()
            .map(|tool| tool.js_name.clone().unwrap_or_else(|| to_codemode_identifier(&tool.name)))
            .collect();
        if script_names.len() != options.tools.len() {
            return Err(SandboxError("Tool script names must be unique".into()));
        }
        check_globals(options.globals.iter().map(|global| global.name.as_str()))?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| SandboxError("start() needs a running Tokio runtime".into()))?;

        let (tools_json, globals_json) = execution::declarations_json(&options.tools, &options.globals);
        let (sender, deliveries) = unbounded_channel();
        let state = Arc::new(Mutex::new(execution::RunState::default()));
        let params = execution::ExecutionParams {
            code: code.into(),
            tools: Vec::new(),
            globals: Vec::new(),
            tools_json,
            globals_json,
            timeout: options.timeout,
            cancel: options.cancel,
            closed: self.closed.clone(),
            memory_limit: self.memory_limit_bytes,
            acquire_worker: self.acquire_worker.clone(),
            store: options.store,
            control: Some(execution::Control {
                mode: worker::Deterministic { started_at: options.started_at, seed: options.seed },
                callbacks: execution::ControlCallbacks {
                    on_started: options.on_started,
                    on_call: options.on_call,
                    on_idle: options.on_idle,
                    state: state.clone(),
                },
                deliveries,
            }),
        };
        let task = AbortOnDrop(runtime.spawn(execution::run(params)));
        let result: BoxFuture<'static, CodemodeResult> = Box::pin(async move {
            let mut task = task;
            (&mut task.0).await.unwrap_or_else(|error| CodemodeResult {
                outcome: Err(CodemodeError::new(CodemodeErrorKind::Sandbox, format!("The run failed: {error}"))),
                output: Vec::new(),
                calls: Vec::new(),
                during: None,
            })
        });
        Ok(CodemodeRun { result, deliverer: RunDeliverer { state, sender } })
    }

    /// Aborts in-flight executions (they fail with `kind: aborted`) and rejects new ones.
    pub fn close(&self) {
        self.closed.cancel();
    }
}

/// Ends the run when its result is dropped.
struct AbortOnDrop(JoinHandle<CodemodeResult>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A running [`CodemodeSandbox::start`] execution.
pub struct CodemodeRun {
    /// The outcome of the run. Resolves for script failures too.
    pub result: BoxFuture<'static, CodemodeResult>,
    deliverer: RunDeliverer,
}

impl CodemodeRun {
    /// See [`RunDeliverer::deliver`].
    pub fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), SandboxError> {
        self.deliverer.deliver(seq, now, id, outcome)
    }

    /// A handle that answers the run's calls, for example from inside `on_call`.
    pub fn deliverer(&self) -> RunDeliverer {
        self.deliverer.clone()
    }

    /// See [`RunDeliverer::finished`].
    pub fn finished(&self) -> bool {
        self.deliverer.finished()
    }
}

impl fmt::Debug for CodemodeRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CodemodeRun")
    }
}

/// Answers the calls of a [`CodemodeSandbox::start`] run.
#[derive(Clone)]
pub struct RunDeliverer {
    state: execution::SharedRunState,
    sender: UnboundedSender<execution::Delivery>,
}

impl RunDeliverer {
    /// Whether the run has its outcome: the script ended, timed out, or failed, or the run was aborted. Set
    /// before [`CodemodeRun::result`] resolves; calls can no longer be answered.
    pub fn finished(&self) -> bool {
        execution::lock_state(&self.state).finished
    }

    /// Answer call `id` as result number `seq`. Results are numbered 1, 2, ... without gaps, and the VM
    /// applies them strictly in that order, whatever order they arrive in: after a gap it waits for the
    /// missing number. While it processes this one, its clock reads `now` (ms since the epoch), or the
    /// previous value if `now` is earlier. Fails for a call that is not waiting for an answer, a `seq`
    /// used before, or a `now` that is not finite. Does nothing once the run has ended.
    pub fn deliver(&self, seq: u64, now: f64, id: u64, outcome: CodemodeOutcome) -> Result<(), SandboxError> {
        let mut state = execution::lock_state(&self.state);
        if state.finished {
            return Ok(());
        }
        if !now.is_finite() {
            return Err(SandboxError(format!("The clock value of result {seq} is not a finite number")));
        }
        if seq < 1 || state.delivered.contains(&seq) {
            return Err(SandboxError(format!("Result number {seq} is not a positive integer used once")));
        }
        if !state.waiting.remove(&id) {
            return Err(SandboxError(format!("Call {id} is not waiting for a result")));
        }
        state.delivered.insert(seq);
        let _ = self.sender.send(execution::Delivery { seq, now, id, outcome });
        Ok(())
    }
}

impl fmt::Debug for RunDeliverer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RunDeliverer")
    }
}
