//! Values that cross between the sandbox and its caller.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::declarations::ToolDeclaration;

/// Output of one script, at most this many characters of text and base64 image data.
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;
/// Output of one script, at most this many `text()`, `image()`, and `console` calls.
pub const MAX_OUTPUT_ITEMS: usize = 100_000;
/// Largest JSON text of one `store()` value.
pub const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;
/// Largest JSON text of all stored values together, keys included.
pub const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;

/// Runs a tool or global. `args` is what the script passed after a JSON round trip (`None` for
/// `undefined`); `Ok(None)` resolves to `undefined`, and `Err(message)` rejects with an `Error`.
pub type SandboxExecute =
    Arc<dyn Fn(Option<Value>, CancellationToken) -> BoxFuture<'static, Result<Option<Value>, String>> + Send + Sync>;

/// A function the script can call: a tool (`tools.<id>(args)`) or a global (`name(args)` or
/// `namespace.member(args)`). The token is cancelled when the script ends.
#[derive(Clone)]
pub struct SandboxFunction {
    pub declaration: ToolDeclaration,
    /// Globals only: the function receives all call arguments as an array.
    pub spread: bool,
    pub execute: SandboxExecute,
}

impl SandboxFunction {
    pub fn new<F, Fut>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Option<Value>, String>> + Send + 'static,
    {
        Self {
            declaration: ToolDeclaration::new(name),
            spread: false,
            execute: Arc::new(move |args, cancel| Box::pin(execute(args, cancel))),
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.declaration.description = Some(description.into());
        self
    }

    pub fn with_spread(mut self) -> Self {
        self.spread = true;
        self
    }

    pub fn name(&self) -> &str {
        &self.declaration.name
    }
}

impl std::fmt::Debug for SandboxFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxFunction").field("declaration", &self.declaration).field("spread", &self.spread).finish()
    }
}

/// Deadline of one execution, including time spent in tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timeout {
    Never,
    After(Duration),
}

/// One item of the script's output, in order: `text()` and `console.*` produce text items
/// (`console: true` for `console.*`), `image()` image items with base64 `data`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CodemodeOutputItem {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        console: bool,
    },
    #[serde(rename_all = "camelCase")]
    Image { data: String, mime_type: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallStatus {
    Ok,
    Error,
    Cancelled,
}

/// A tool call the script made. Globals are not recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodemodeCall {
    pub name: String,
    pub status: CodemodeCallStatus,
    pub duration_ms: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeErrorKind {
    /// The script threw or failed to parse. `name` and `stack` come from the script's error.
    Script,
    /// The deadline expired.
    Timeout,
    /// The caller cancelled or the sandbox was closed.
    Aborted,
    /// The VM or its bridge failed outside the script's control.
    Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodemodeError {
    pub kind: CodemodeErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
}

impl CodemodeError {
    pub(crate) fn new(kind: CodemodeErrorKind, message: impl Into<String>) -> Self {
        Self { kind, name: None, message: message.into(), stack: None }
    }
}

/// Keys the script changed with `store()`. Only successful executions report writes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CodemodeStoreWrites {
    pub set: Map<String, Value>,
    /// Keys stored as `undefined`.
    pub delete: Vec<String>,
}

impl CodemodeStoreWrites {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty() && self.delete.is_empty()
    }
}

/// A completed script: its return value (`None` for `undefined`, also after `exit()`) and writes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CodemodeSuccess {
    pub value: Option<Value>,
    pub store_writes: CodemodeStoreWrites,
}

/// In a [`CodemodeSandbox::start`](super::CodemodeSandbox::start) run: the result the VM was processing
/// when the script finished (0: its first run) and the calls issued by then.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodemodeDuring {
    pub seq: u64,
    pub issued: u64,
}

/// `output` and `calls` are kept for failed executions too, up to the failure. `during` is set only
/// when a [`start`](super::CodemodeSandbox::start) run ended by itself (it returned, threw, or exited).
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeResult {
    pub outcome: Result<CodemodeSuccess, CodemodeError>,
    pub output: Vec<CodemodeOutputItem>,
    pub calls: Vec<CodemodeCall>,
    pub during: Option<CodemodeDuring>,
}

impl CodemodeResult {
    pub fn is_ok(&self) -> bool {
        self.outcome.is_ok()
    }

    /// The return value of a successful script.
    pub fn value(&self) -> Option<&Value> {
        self.outcome.as_ref().ok().and_then(|success| success.value.as_ref())
    }

    pub fn error(&self) -> Option<&CodemodeError> {
        self.outcome.as_ref().err()
    }
}

/// Options of one [`CodemodeSandbox::execute`](super::CodemodeSandbox::execute) call.
#[derive(Clone, Debug, Default)]
pub struct ExecuteOptions {
    /// Cancels the execution, which then fails as `aborted`.
    pub cancel: Option<CancellationToken>,
    /// Overrides the sandbox default for this execution.
    pub timeout: Option<Timeout>,
    /// Values the script reads with `load(key)`.
    pub store: Map<String, Value>,
}

/// What a call in a [`start`](super::CodemodeSandbox::start) run targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodemodeCallTarget {
    Tool,
    Global,
}

/// A call the script made in a [`start`](super::CodemodeSandbox::start) run, answered with
/// [`CodemodeRun::deliver`](super::CodemodeRun::deliver).
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeHostCall {
    /// The VM's own number for the call: 1, 2, ... in the order the script made its calls (tools and globals).
    pub id: u64,
    pub target: CodemodeCallTarget,
    pub name: String,
    /// What the script passed after a JSON round trip (`None` for `undefined`); for spread globals the argument list.
    pub args: Option<Value>,
}

/// A tool as a [`start`](super::CodemodeSandbox::start) run's script sees it: `tools.<name>` and its
/// `ALL_TOOLS` entry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodemodeDeclaredTool {
    pub name: String,
    /// The identifier the script uses, `tools.<js_name>`. Default: `to_codemode_identifier(name)`. A host that
    /// replays a run passes the one the run started with, so a changed mapping cannot rename a tool under a
    /// resumed script.
    pub js_name: Option<String>,
    pub description: Option<String>,
}

impl CodemodeDeclaredTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }
}

/// A global as a [`start`](super::CodemodeSandbox::start) run's script sees it; the same names as
/// [`SandboxOptions::globals`](super::SandboxOptions::globals) allow.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodemodeDeclaredGlobal {
    pub name: String,
    pub spread: bool,
}

impl CodemodeDeclaredGlobal {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), spread: false }
    }
}

/// A call's answer. A rejection becomes an `Error` with `message` in the script, carrying the own
/// properties of `properties` (JSON values; `name`, `message`, and `stack` are ignored), for example
/// `{ "structuredOutput": ... }`.
#[derive(Clone, Debug, PartialEq)]
pub enum CodemodeOutcome {
    Ok(Option<Value>),
    Err { message: String, properties: Option<Map<String, Value>> },
}

/// Called with the VM's fingerprint, before the script runs and before any of its calls.
pub type OnStarted = Arc<dyn Fn(&str) + Send + Sync>;
/// Called when the script made a call. Answer it with [`CodemodeRun::deliver`](super::CodemodeRun::deliver),
/// now or later.
pub type OnCall = Arc<dyn Fn(CodemodeHostCall) + Send + Sync>;
/// Called when the VM processed result `seq` (0: the script's first run, before any result) and waits
/// again, having issued `issued` calls in total.
pub type OnIdle = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// Options of one [`CodemodeSandbox::start`](super::CodemodeSandbox::start) call.
#[derive(Clone)]
pub struct CodemodeRunOptions {
    /// The VM clock's start, in ms since the epoch: `Date.now()` until the first result arrives.
    pub started_at: f64,
    /// Seeds `Math.random()`.
    pub seed: u32,
    pub tools: Vec<CodemodeDeclaredTool>,
    pub globals: Vec<CodemodeDeclaredGlobal>,
    /// Values the script reads with `load(key)`.
    pub store: Map<String, Value>,
    /// Ends the run with an error the script cannot catch; it then fails as `aborted`.
    pub cancel: Option<CancellationToken>,
    /// Deadline of this run. Default: none (not the sandbox's default).
    pub timeout: Timeout,
    pub on_started: Option<OnStarted>,
    pub on_call: OnCall,
    pub on_idle: Option<OnIdle>,
}

impl CodemodeRunOptions {
    pub fn new(started_at: f64, seed: u32, on_call: impl Fn(CodemodeHostCall) + Send + Sync + 'static) -> Self {
        Self {
            started_at,
            seed,
            tools: Vec::new(),
            globals: Vec::new(),
            store: Map::new(),
            cancel: None,
            timeout: Timeout::Never,
            on_started: None,
            on_call: Arc::new(on_call),
            on_idle: None,
        }
    }
}

impl std::fmt::Debug for CodemodeRunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeRunOptions")
            .field("started_at", &self.started_at)
            .field("seed", &self.seed)
            .field("tools", &self.tools)
            .field("globals", &self.globals)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}
