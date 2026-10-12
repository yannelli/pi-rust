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

/// `output` and `calls` are kept for failed executions too, up to the failure.
#[derive(Clone, Debug, PartialEq)]
pub struct CodemodeResult {
    pub outcome: Result<CodemodeSuccess, CodemodeError>,
    pub output: Vec<CodemodeOutputItem>,
    pub calls: Vec<CodemodeCall>,
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
