//! Durable code mode (upstream `@earendil-works/pi-durable/codemode`): a `codemode` tool whose model-written scripts
//! call the conversation's tools as nested calls and survive a restart, by deterministic replay of a journal of the
//! results each script got, written before each delivery.

mod call;
mod details;
mod docs;
mod outcome;
mod replay;
mod run;

use std::sync::Arc;

use futures::future::BoxFuture;
use pi_chord::Context;
use pi_codemode::{CodemodeRun, CodemodeRunOptions, CodemodeSandbox, SandboxError};
use serde_json::{Value, json};

pub use details::{CodemodeNestedCall, CodemodeNestedCallStatus, CodemodeToolDetails};
pub use docs::{
    CODEMODE_JOURNAL_DOC, CODEMODE_RUN_DOC, CODEMODE_STORE_DOC, CodemodeStart, DeclaredGlobal, DeclaredTool,
    JournalEntry, JournalGlobal, JournalState, RunState, StoreState, StoredResult,
};

use crate::errors::{DurableError, DurableResult};
use crate::harness::{Extension, ToolCaller, ToolExecutionApi, ToolRegistration, ToolReplay, define_tool};

/// Version of everything in this module that turns stored results into what a script sees: result mapping, globals,
/// start data. Bump it when that changes, so a call started before an upgrade is not replayed by different code.
pub const CODEMODE_MODULE_VERSION: u32 = 1;

pub const CODEMODE_TOOL_NAME: &str = "codemode";

/// Runs scripts with pi-codemode's deterministic `start()` (upstream `CodemodeSandboxBase`).
pub trait CodemodeSandboxBase: Send + Sync {
    fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError>;
}

impl CodemodeSandboxBase for CodemodeSandbox {
    fn start(&self, code: &str, options: CodemodeRunOptions) -> Result<CodemodeRun, SandboxError> {
        CodemodeSandbox::start(self, code, options)
    }
}

/// Runs a global: the script's arguments (`None` for `undefined`); `Ok(None)` resolves to `undefined`.
pub type CodemodeGlobalFn = Arc<
    dyn Fn(Option<Value>, ToolExecutionApi, Context) -> BoxFuture<'static, DurableResult<Option<Value>>> + Send + Sync,
>;

/// A function scripts call as a top-level global; its result is journaled.
#[derive(Clone)]
pub struct CodemodeGlobal {
    pub name: String,
    pub spread: bool,
    pub execute: CodemodeGlobalFn,
}

impl CodemodeGlobal {
    pub fn new<F, Fut>(name: impl Into<String>, execute: F) -> Self
    where
        F: Fn(Option<Value>, ToolExecutionApi, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = DurableResult<Option<Value>>> + Send + 'static,
    {
        Self {
            name: name.into(),
            spread: false,
            execute: Arc::new(move |args, api, ctx| Box::pin(execute(args, api, ctx))),
        }
    }

    pub fn with_spread(mut self) -> Self {
        self.spread = true;
        self
    }
}

impl std::fmt::Debug for CodemodeGlobal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodemodeGlobal").field("name", &self.name).field("spread", &self.spread).finish()
    }
}

#[derive(Clone)]
pub struct CodemodeToolOptions {
    /// Runs the scripts, usually a [`CodemodeSandbox`].
    pub sandbox: Arc<dyn CodemodeSandboxBase>,
    /// Host functions scripts call as globals.
    pub globals: Vec<CodemodeGlobal>,
}

impl CodemodeToolOptions {
    pub fn new(sandbox: Arc<dyn CodemodeSandboxBase>) -> Self {
        Self { sandbox, globals: Vec::new() }
    }
}

fn codemode_parameters() -> Value {
    json!({
        "type": "object",
        "properties": { "code": { "type": "string", "description": "Raw JavaScript source." } },
        "required": ["code"],
    })
}

const CODEMODE_DESCRIPTION: &str = "Run JavaScript that calls other tools. The input is raw JavaScript, run as an async function body in a QuickJS sandbox: top-level `await` and `return` work. No Node, file system, network, or timers. `await tools.<name>({ ...args })` resolves to the tool's result and rejects with an Error on failure. `text(value)` and `console.log()` add output.";

/// The code mode tool. Scripts call the conversation's callable tools as `tools.<name>(args)`.
pub fn create_codemode_tool(options: CodemodeToolOptions) -> ToolRegistration {
    let options = Arc::new(options);
    define_tool(CODEMODE_TOOL_NAME, CODEMODE_DESCRIPTION, codemode_parameters(), move |args, api, context| {
        let options = options.clone();
        async move {
            let Some(Value::String(code)) = args.get("code") else {
                return Err(DurableError::type_error("codemode expects a code string"));
            };
            run::execute(options, api, context, code).await
        }
    })
    .with_callers(vec![ToolCaller::Model])
    .with_replay(ToolReplay::Safe)
}

/// The `codemode` extension: the code mode tool.
pub fn create_codemode_extension(options: CodemodeToolOptions) -> Extension {
    Extension::new("codemode").tool(create_codemode_tool(options))
}
